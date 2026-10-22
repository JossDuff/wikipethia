// Copy buttons for the connect snippets. Progressive enhancement: the page
// is complete without this file. It reveals the buttons only when the
// Clipboard API exists (secure contexts: https, or localhost when checking
// the page before deploying it).
(function () {
  if (!navigator.clipboard) return;
  document.documentElement.classList.add("js");
  for (const button of document.querySelectorAll("button.copy")) {
    const code = button.closest(".snippet")?.querySelector("code");
    if (!code) continue;
    button.addEventListener("click", async () => {
      try {
        await navigator.clipboard.writeText(code.textContent.trim());
      } catch {
        return; // the text is still selectable; leave the button as it was
      }
      const label = button.textContent;
      button.textContent = "Copied";
      setTimeout(() => { button.textContent = label; }, 1500);
    });
  }
})();
