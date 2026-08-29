(() => {
  const landing = document.querySelector(".sb-landing");
  if (!landing) return;

  document.body.classList.add("sb-landing-active");

  const reduceMotion = window.matchMedia("(prefers-reduced-motion: reduce)").matches;

  const copyText = async (button) => {
    const value = button.dataset.copy;
    if (!value) return;

    try {
      await navigator.clipboard.writeText(value);
      const original = button.textContent;
      button.textContent = "copied";
      window.setTimeout(() => {
        button.textContent = original;
      }, 1400);
    } catch {
      button.textContent = "select manually";
    }
  };

  landing.querySelectorAll("[data-copy]").forEach((button) => {
    button.addEventListener("click", () => copyText(button));
  });

  const revealItems = [...landing.querySelectorAll(".sb-reveal")];
  const reveal = (item) => item.classList.add("is-visible");

  if (reduceMotion || !("IntersectionObserver" in window)) {
    revealItems.forEach(reveal);
  } else {
    const revealObserver = new IntersectionObserver(
      (entries, observer) => {
        entries.forEach((entry) => {
          if (!entry.isIntersecting) return;
          reveal(entry.target);
          observer.unobserve(entry.target);
        });
      },
      { rootMargin: "0px 0px -10% 0px", threshold: 0.05 },
    );
    revealItems.forEach((item) => revealObserver.observe(item));
  }

  const story = landing.querySelector("[data-story-root]");
  const steps = story ? [...story.querySelectorAll(".sb-story-step[data-stage]")] : [];
  if (!story || steps.length === 0) return;

  const setStage = (stage) => {
    story.dataset.stage = stage;
    steps.forEach((step) => {
      step.classList.toggle("is-active", step.dataset.stage === stage);
    });
  };

  setStage(steps[0].dataset.stage || "shape");

  if (reduceMotion || !("IntersectionObserver" in window)) return;

  const stageObserver = new IntersectionObserver(
    (entries) => {
      const visible = entries
        .filter((entry) => entry.isIntersecting)
        .sort((a, b) => b.intersectionRatio - a.intersectionRatio)[0];
      if (visible?.target.dataset.stage) setStage(visible.target.dataset.stage);
    },
    { rootMargin: "-38% 0px -38% 0px", threshold: [0.1, 0.35, 0.7] },
  );

  steps.forEach((step) => stageObserver.observe(step));
})();
