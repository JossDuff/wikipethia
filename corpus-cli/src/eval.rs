//! The retrieval eval: recall@10 over the hand-written question set.
//!
//! `tests/eval/questions.toml` pairs questions with the document ids that
//! should surface. Run this before and after any change to chunking,
//! ranking, or embeddings, and report the delta — a retrieval change
//! without an eval run is not a finished change.

use anyhow::{Context, bail};
use corpus_core::{Document, Store};
use serde::Deserialize;

/// Results per query the metric is computed over.
pub const K: usize = 10;

/// Turns a question into a query vector; `None` runs the eval lexical-only.
pub type EmbedQuery<'a> = &'a dyn Fn(&str) -> anyhow::Result<Vec<f32>>;

#[derive(Deserialize)]
struct QuestionFile {
    questions: Vec<Question>,
}

#[derive(Deserialize)]
pub struct Question {
    pub question: String,
    /// Full document ids expected in the top [`K`], e.g.
    /// `ethresearch/post/1249`. **All-of**: every id counts separately, so a
    /// question with three expects scores 1/3 for each one found.
    ///
    /// Optional so a question can use `expect_any` alone, as that field's
    /// doc comment promises. Without the default, serde rejected the file
    /// before `parse_questions` could run its own checks, and every widened
    /// question had to carry a placeholder `expect = []`.
    #[serde(default)]
    pub expect: Vec<String>,
    /// Groups of interchangeable sources. **Any-of**: each inner group is
    /// worth one credit, earned by finding *any* one of its members.
    ///
    /// This exists because single-document expects were understating the
    /// system, provably rather than arguably. On the 2026-08-14 opus
    /// agent-eval run, all eight questions scoring 0.00 strict had cited
    /// real, on-topic sources — just not the one named in `expect`. "Why does
    /// Ethereum have blobs?" cited EIP-4844 and Vitalik's blobs post where
    /// the expect names the EthMagicians thread; the excess-blob-gas question
    /// scored 1.00 on retrieval and 0.00 on the agent layer for citing
    /// EIP-4844, EIP-7691 and `numeric.py` instead of the single
    /// `cancun/vm/gas.py` named. In several the model's sourcing was better
    /// than the expect. That is a defect in the measure, not the system.
    ///
    /// Deliberately a separate field rather than a reinterpretation of
    /// `expect`: every existing question keeps its exact meaning, so the
    /// recorded baselines stay comparable. A question may use either or both;
    /// the two contribute to one score.
    #[serde(default)]
    pub expect_any: Vec<Vec<String>>,
}

impl Question {
    /// Every id the question references, from both fields — the preflight's
    /// input, so a typo anywhere fails loudly rather than deflating a score.
    pub fn all_ids(&self) -> impl Iterator<Item = &String> {
        self.expect.iter().chain(self.expect_any.iter().flatten())
    }

    /// How many credits a perfect answer earns: one per `expect` id, plus one
    /// per `expect_any` group.
    fn credits(&self) -> usize {
        self.expect.len() + self.expect_any.len()
    }
}

pub fn parse_questions(text: &str) -> anyhow::Result<Vec<Question>> {
    let file: QuestionFile = toml::from_str(text).context("parsing questions file")?;
    if file.questions.is_empty() {
        bail!("questions file contains no [[questions]] entries");
    }
    for q in &file.questions {
        if q.question.trim().is_empty() {
            bail!("a question is empty");
        }
        if q.expect.is_empty() && q.expect_any.is_empty() {
            bail!(
                "question {:?} has neither expect nor expect_any — it can never \
                 score anything",
                q.question
            );
        }
        // The same id in both fields would be counted twice, quietly
        // reweighting the question toward one document.
        let mut seen = std::collections::HashSet::new();
        for id in q.all_ids() {
            if !seen.insert(id) {
                bail!(
                    "question {:?} lists {id:?} more than once across expect / \
                     expect_any — it would earn credit twice",
                    q.question
                );
            }
        }
        if q.expect_any.iter().any(Vec::is_empty) {
            bail!(
                "question {:?} has an empty expect_any group — an alternatives \
                 group with no alternatives is unsatisfiable and would silently \
                 cap the question's score",
                q.question
            );
        }
    }
    Ok(file.questions)
}

/// One question's expected documents, keeping the two scoring shapes apart.
pub struct ExpectedDocs {
    /// One credit each — all of them are wanted.
    pub required: Vec<Document>,
    /// One credit per group, earned by any single member.
    pub alternatives: Vec<Vec<Document>>,
}

/// Every question's expected docs, resolved. An id that is not in the corpus
/// at all is a typo in the questions file, not a retrieval miss — fail loudly
/// instead of silently deflating the score. The one preflight both eval
/// layers share; retrieval eval ignores the payload.
pub fn resolve_expected_docs(
    store: &Store,
    questions: &[Question],
) -> anyhow::Result<Vec<ExpectedDocs>> {
    let resolve = |q: &Question, id: &String| -> anyhow::Result<Document> {
        store.get(id)?.ok_or_else(|| {
            anyhow::anyhow!(
                "expect id {id:?} (question {:?}) is not in the corpus — \
                 typo, or the topic is not synced?",
                q.question
            )
        })
    };
    questions
        .iter()
        .map(|q| {
            Ok(ExpectedDocs {
                required: q
                    .expect
                    .iter()
                    .map(|id| resolve(q, id))
                    .collect::<anyhow::Result<_>>()?,
                alternatives: q
                    .expect_any
                    .iter()
                    .map(|group| group.iter().map(|id| resolve(q, id)).collect())
                    .collect::<anyhow::Result<_>>()?,
            })
        })
        .collect()
}

/// A question's recall over `got`: one credit per `expect` id found, plus one
/// per `expect_any` group with any member found, over the total available.
/// The order of `got` does not matter at fixed k.
pub fn recall(question: &Question, got: &[String]) -> f64 {
    let required = question
        .expect
        .iter()
        .filter(|id| got.contains(id))
        .count();
    let alternatives = question
        .expect_any
        .iter()
        .filter(|group| group.iter().any(|id| got.contains(id)))
        .count();
    (required + alternatives) as f64 / question.credits() as f64
}

/// Run both rankings per question — lexical (BM25 only) and fused (hybrid,
/// when `embed_query` is available) — so every eval run reports the delta
/// retrieval changes are judged by.
pub fn run(
    store: &Store,
    questions: &[Question],
    embed_query: Option<EmbedQuery<'_>>,
) -> anyhow::Result<()> {
    resolve_expected_docs(store, questions)?;

    let mut lex_total = 0.0;
    let mut fused_total = 0.0;
    println!("  lex fused  question");
    for q in questions {
        let lex: Vec<String> = store
            .search(&q.question, K)?
            .into_iter()
            .map(|hit| hit.doc_id)
            .collect();
        let lex_recall = recall(q, &lex);
        lex_total += lex_recall;
        let fused_recall = match embed_query {
            Some(embed) => {
                let vector = embed(&q.question)?;
                let got: Vec<String> = store
                    .hybrid_search(&q.question, Some(&vector), K)?
                    .into_iter()
                    .map(|hit| hit.doc_id)
                    .collect();
                recall(q, &got)
            }
            None => lex_recall,
        };
        fused_total += fused_recall;
        println!(" {lex_recall:.2}  {fused_recall:.2}  {}", q.question);
    }
    let n = questions.len() as f64;
    let (lex_mean, fused_mean) = (lex_total / n, fused_total / n);
    println!(
        "\nmean recall@{K}: lexical {lex_mean:.3}, fused {fused_mean:.3} (Δ {:+.3}) over {} questions",
        fused_mean - lex_mean,
        questions.len()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_valid_file() {
        let text = r#"
            [[questions]]
            question = "How does minimal viable plasma handle exit games?"
            expect = ["ethresearch/post/1249", "ethresearch/post/1865"]

            [[questions]]
            question = "second"
            expect = ["ethresearch/post/8"]
        "#;
        let questions = parse_questions(text).unwrap();
        assert_eq!(questions.len(), 2);
        assert_eq!(questions[0].expect.len(), 2);
    }

    #[test]
    fn rejects_empty_or_incomplete_files() {
        assert!(parse_questions("").is_err());
        assert!(parse_questions("questions = []").is_err());
        let no_expect = "[[questions]]\nquestion = \"q\"\nexpect = []";
        assert!(parse_questions(no_expect).is_err());
        let blank_question = "[[questions]]\nquestion = \" \"\nexpect = [\"x\"]";
        assert!(parse_questions(blank_question).is_err());
    }

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn question(expect: &[&str], expect_any: &[&[&str]]) -> Question {
        Question {
            question: "q".into(),
            expect: ids(expect),
            expect_any: expect_any.iter().map(|g| ids(g)).collect(),
        }
    }

    #[test]
    fn recall_counts_expected_ids_found() {
        let q = question(&["a", "b"], &[]);
        assert_eq!(recall(&q, &ids(&["a", "b", "c"])), 1.0);
        assert_eq!(recall(&q, &ids(&["a"])), 0.5);
        assert_eq!(recall(&q, &[]), 0.0);
        // Duplicates in results don't double-count.
        assert_eq!(recall(&q, &ids(&["a", "a"])), 0.5);
    }

    /// A group is one credit earned by any single member — the whole point
    /// of the field. Citing two members is not worth more than citing one.
    #[test]
    fn an_expect_any_group_is_one_credit_for_any_member() {
        let q = question(&[], &[&["a", "b", "c"]]);
        assert_eq!(recall(&q, &ids(&["a"])), 1.0);
        assert_eq!(recall(&q, &ids(&["c"])), 1.0);
        assert_eq!(recall(&q, &ids(&["a", "b", "c"])), 1.0);
        assert_eq!(recall(&q, &ids(&["z"])), 0.0);
    }

    #[test]
    fn required_and_alternative_credits_share_one_score() {
        // 1 required + 2 groups = 3 credits.
        let q = question(&["r"], &[&["a1", "a2"], &["b1", "b2"]]);
        assert_eq!(recall(&q, &ids(&["r", "a2", "b1"])), 1.0);
        assert!((recall(&q, &ids(&["r"])) - 1.0 / 3.0).abs() < 1e-9);
        assert!((recall(&q, &ids(&["a1", "b2"])) - 2.0 / 3.0).abs() < 1e-9);
    }

    /// The baselines recorded in ROADMAP.md were measured before this field
    /// existed; a question that does not use it must score exactly as it did.
    #[test]
    fn questions_without_expect_any_are_unchanged() {
        let text = "[[questions]]\nquestion = \"q\"\nexpect = [\"a\", \"b\"]";
        let parsed = parse_questions(text).unwrap();
        assert!(parsed[0].expect_any.is_empty());
        assert_eq!(recall(&parsed[0], &ids(&["a"])), 0.5);
    }

    #[test]
    fn parses_and_validates_expect_any() {
        let text = r#"
            [[questions]]
            question = "Why does Ethereum have blobs?"
            expect = []
            expect_any = [["ethmagicians/post/23602", "eips/eip-4844"]]
        "#;
        let q = &parse_questions(text).unwrap()[0];
        assert_eq!(q.expect_any.len(), 1);
        assert_eq!(q.all_ids().count(), 2);

        // `expect` omitted entirely, which is what the doc comment promises
        // and what every widened question in questions.toml now does. Without
        // `#[serde(default)]` on `expect`, serde rejected the whole file
        // before any of the validation below could run.
        let groups_only = "[[questions]]\nquestion = \"q\"\nexpect_any = [[\"a\"]]";
        let q = &parse_questions(groups_only).unwrap()[0];
        assert!(q.expect.is_empty());
        assert_eq!(recall(q, &ids(&["a"])), 1.0);

        // An empty group can never be satisfied — it would silently cap the
        // question below 1.00 for ever, which is the failure this guards.
        let empty_group =
            "[[questions]]\nquestion = \"q\"\nexpect = []\nexpect_any = [[]]";
        assert!(parse_questions(empty_group).is_err());
        // Neither field means the question can never score.
        let neither = "[[questions]]\nquestion = \"q\"\nexpect = []";
        assert!(parse_questions(neither).is_err());
    }
}
