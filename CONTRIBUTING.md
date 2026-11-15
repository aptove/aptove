# Contributing to Aptove

Thank you for your interest in contributing to Aptove.

---

## Bug Reports

**Minor or clearly reproducible bugs** — open a [GitHub Issue](https://github.com/aptove/aptove/issues) with steps to reproduce, your OS, and the Aptove version.

**Complex or architectural bugs** — open a GitHub Issue describing the behaviour, and optionally submit an RFC in `rfc/` (see below) with a problem description and proposed approach if the fix touches core design.

---

## Security Issues

**Do NOT open a public GitHub Issue or pull request for security vulnerabilities.**

Security issues reported publicly can be exploited before a fix is available and may put existing users at risk. Instead, follow the responsible disclosure process in [SECURITY.md](SECURITY.md).

---

## Feature & Enhancement Proposals (RFC)

Large or design-significant changes go through the RFC process before any code is written. This keeps review focused and prevents wasted implementation effort.

### How to submit an RFC

1. Create a folder under `rfc/` using a short kebab-case name:
   ```
   rfc/multi-agent-sessions/
   ```

2. Add a `proposal.md` file following the template at [`rfc/example/sample.md`](rfc/example/sample.md).

3. Open a pull request with just that folder — **no implementation code at the RFC stage**.

4. The maintainers will label the PR `rfc: under-review` and leave feedback. The RFC will eventually be labelled `rfc: accepted` or `rfc: declined`.

### RFC content guidelines

- **Problem description** — what is broken or missing, and who is affected.
- **High-level architecture** — components, data flow, key decisions. Diagrams or ASCII art are welcome.
- **No code required** — pseudocode or interface sketches are fine; working implementations are not expected.

### After acceptance

- Accepted RFCs get a linked tracking GitHub Issue for implementation.
- The RFC author is credited and gets the first opportunity to implement the feature.

---

## RFC Review Labels

| Label | Meaning |
|-------|---------|
| `rfc: under-review` | Maintainers are evaluating the proposal |
| `rfc: accepted` | Proposal approved; implementation tracked in a linked issue |
| `rfc: declined` | Proposal not accepted; reason given in the PR thread |
