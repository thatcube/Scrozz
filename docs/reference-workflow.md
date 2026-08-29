# UI reference workflow

Scrozz uses competitor screenshots to understand behavior and calibrate quality
without copying product assets or visual designs. The private reference library
lives at:

```text
~/.copilot/scrozz-ui-reference/
```

Start with `INDEX.md`. It maps descriptive filenames to the behavior each image
demonstrates. The complete library stays outside the repository under D17.

## Before designing

1. Find the feature ID in [`feature-audit.md`](feature-audit.md).
2. Read the corresponding decisions in [`decisions.md`](decisions.md).
3. Open only the relevant curated references from the private index.
4. Write a Scrozz contract in product language:
   - the user's goal;
   - visible states and transitions;
   - information hierarchy;
   - keyboard, pointer, accessibility and reduced-motion behavior;
   - failure, cancellation and constrained-window behavior;
   - platform adaptations.
5. Identify what should be improved rather than inherited.

A screenshot by itself is never an implementation specification. Agent prompts
must name the reference files and include the translated contract.

## While implementing

- Use Scrozz design tokens and Tabler Icons.
- Preserve native operating-system conventions where they differ.
- Derive dimensions from Scrozz's spacing, typography and density tokens.
- Do not sample competitor colors, trace icons, copy wording, or reproduce an
  exact layout.
- Keep competitor images out of source, tests, fixtures, generated assets and
  shipped bundles.
- Store Scrozz bug screenshots separately under the private `feedback/` folder;
  they are reproduction evidence, not a desired visual target.

## Before accepting

1. Render light, dark, compact and relevant platform states through the real
   deterministic UI harness.
2. Compare hierarchy, clarity, density and interaction coverage with the
   reference—not pixel similarity.
3. Test keyboard navigation, screen-reader names, high contrast, scaling and
   reduced motion.
4. Exercise the feature in a real native session.
5. Record any intentional difference and why it is better for Scrozz.

Generated Scrozz goldens are the regression authority. Competitor screenshots
remain research evidence only.

## Adding new references

1. Preserve the original unchanged in a dated private `inbox/` folder.
2. Copy useful images to a descriptive curated path.
3. Add the path and observed behavior to the private `INDEX.md`.
4. Commit an image under `docs/reference/` only when a specific audit claim
   cannot be understood without it, and include attribution and scope.
