# How Scrozz is built, and what it does not do

Two different questions get confused with each other whenever AI comes up in an
open-source project, so this document answers them separately and plainly.

1. **Was AI used to build it?** Yes, for implementation, under direction.
2. **Does the app use AI?** No. Scrozz ships no generative AI, sends nothing
   anywhere, and has no account.

The short version, which is the one worth remembering:

> **Built with AI assistance, not powered by AI.**

---

## Who makes the decisions

**Brandon Moore** conceived Scrozz, did the research, designed the product and
its visual identity, made every architectural decision, tested the results, and
maintains it. He is the founder, product designer, researcher, and maintainer,
and the project is his.

Coding agents assisted with implementation — writing code against decisions that
were already made, and building the automated checks that prove that code does
what it claims. They are a tool in the same category as a compiler or a
refactoring script: powerful, useful, and not the author of the work.

This is not an aspiration written after the fact. It is the operating rule the
project was started with, recorded as
[decision D5](docs/decisions.md) before most of the code existed:

> **Agents implement and validate; the maintainer gates shipping.** Agents write
> the code and build the automated validation that proves it works. The
> maintainer verifies personally — by testing or by inspection — before anything
> ships.

Two more decisions exist specifically because an agent's judgement is not
trusted by default:

- **[D25](docs/decisions.md) — every screenshot is generated, none is taken by
  hand.** The reason is stated bluntly in the decision itself: *an agent cannot
  see*. During an early spike, an agent painted an unrounded scrim over a
  rounded thumbnail, squaring the bottom corners, and never noticed; Brandon
  caught it in seconds. Golden-image tests turn "a human has to look at it" into
  "the build fails," which is the only way unattended UI work is safe.
- **[D1](docs/decisions.md) — clean-room implementation.** No competitor source
  is copied, ever. [D24](docs/decisions.md) goes further and keeps competitor
  names out of every artifact an implementer reads, precisely because an agent
  handed a competitor's name will reach for that product's behaviour as the
  specification instead of building what Scrozz should do.

Every commit in this repository is authored by Brandon Moore. That is a fact you
can check with `git shortlog -sne`, and it reflects who is accountable for the
result.

## What Scrozz does not do

These are properties of the shipped application, not promises about intent.

- **No generative AI at runtime.** Scrozz contains no large language model or
  diffusion model. It does not describe, summarise, or generate capture content.
  An explicit OCR command does interpret image pixels as text locally; that
  narrow recognition step is the only model-backed processing.
- **Your captures are never uploaded to an AI service.** There is no such
  integration, and none is planned.
- **No telemetry, analytics, crash reporting, or usage tracking.**
- **No account, no sign-in, no server.** There is no Scrozz backend to talk to.
- **Your data is not monetised.** There is nothing to monetise, because none of
  it leaves your machine.
- **Text recognition is local.** Optical character recognition uses Vision on
  macOS, `Windows.Media.Ocr` when the Windows process has package identity, and
  an installed Tesseract executable for Linux and the portable Windows ZIP.
  Scrozz bundles no OCR model, passes an explicit installed language model, and
  returns an actionable error when the required executable or language data is
  absent. Nothing is sent off the machine.

### How you can verify that

You do not have to take any of it on trust:

- **There is no HTTP client in the dependency tree.** Search `Cargo.lock` for
  `reqwest`, `hyper`, `ureq`, or `curl` and you will find none. An application
  that cannot make a web request cannot phone home.
- **The source is GPL-3.0** (see [`LICENSE`](LICENSE)), so every line is
  readable, and any derivative must publish its own source too.
- **The dependency set is audited in CI.** `cargo-deny` checks licences and
  security advisories on every push; see
  [`.github/workflows/supply-chain.yml`](.github/workflows/supply-chain.yml).

## Why say any of this

"AI-assisted" is doing a lot of unhappy work in software right now. It is used
to mean anything from *a person used autocomplete* to *this product exists to
harvest what you feed it*. Users of a screenshot tool have a specific and
entirely reasonable worry — a screenshot tool sees your screen, including things
you never meant to share — and they deserve a direct answer rather than a
marketing adjective.

So: an AI helped write this code, under direction, with the checks to prove it
works. The app itself is an ordinary native program that reads your screen when
you ask it to and writes a file. That is the whole story.

---

*Questions, or something here that reads as overstated? Please
[open an issue](https://github.com/thatcube/scrozz/issues) — an inaccurate
honesty document is worse than none.*
