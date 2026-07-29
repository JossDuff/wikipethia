//! The retrieval eval: recall@10 over the hand-written question set.
//!
//! `tests/eval/questions.toml` pairs questions with the document ids that
//! should surface. Run this before and after any change to chunking,
//! ranking, or embeddings, and report the delta — a retrieval change
//! without an eval run is not a finished change.

use anyhow::{Context, bail};
use corpus_core::Store;
use serde::Deserialize;

/// Results per query the metric is computed over.
pub const K: usize = 10;

#[derive(Deserialize)]
struct QuestionFile {
    questions: Vec<Question>,
}

#[derive(Deserialize)]
pub struct Question {
    pub question: String,
    /// Full document ids expected in the top [`K`], e.g.
    /// `ethresearch/post/1249`.
    pub expect: Vec<String>,
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
        if q.expect.is_empty() {
            bail!("question {:?} has an empty expect list", q.question);
        }
    }
    Ok(file.questions)
}

/// Fraction of `expect` ids present in `got` — the order of `got` does not
/// matter at fixed k.
pub fn recall_at_k(expect: &[String], got: &[String]) -> f64 {
    let found = expect.iter().filter(|id| got.contains(id)).count();
    found as f64 / expect.len() as f64
}

pub fn run(store: &Store, questions: &[Question]) -> anyhow::Result<()> {
    // An expect id that is not in the corpus at all is a typo in the
    // questions file, not a retrieval miss — fail loudly instead of
    // silently deflating the score.
    for q in questions {
        for id in &q.expect {
            if store.get(id)?.is_none() {
                bail!(
                    "expect id {id:?} (question {:?}) is not in the corpus — \
                     typo, or the topic is not synced?",
                    q.question
                );
            }
        }
    }

    let mut total = 0.0;
    println!("recall@{K}  question");
    for q in questions {
        let got: Vec<String> = store
            .search(&q.question, K)?
            .into_iter()
            .map(|hit| hit.doc_id)
            .collect();
        let recall = recall_at_k(&q.expect, &got);
        total += recall;
        println!("     {recall:.2}  {}", q.question);
    }
    let mean = total / questions.len() as f64;
    println!(
        "\nmean recall@{K}: {mean:.3} over {} questions",
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

    #[test]
    fn recall_counts_expected_ids_found() {
        let expect = vec!["a".to_string(), "b".to_string()];
        let got: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        assert_eq!(recall_at_k(&expect, &got), 1.0);
        assert_eq!(recall_at_k(&expect, &got[..1]), 0.5);
        assert_eq!(recall_at_k(&expect, &[]), 0.0);
        // Duplicates in results don't double-count.
        let dup: Vec<String> = ["a", "a"].iter().map(|s| s.to_string()).collect();
        assert_eq!(recall_at_k(&expect, &dup), 0.5);
    }
}
