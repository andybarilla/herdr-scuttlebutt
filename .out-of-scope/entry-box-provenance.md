# Entry-Box Provenance

This project does not attempt to determine whether text in a pane's entry box was typed by a person.

## Why this is out of scope

The signal would have to come from herdr, a third party. Their `CONTRIBUTING` closes unsolicited pull requests automatically, "regardless of their size, title, test results, or whether a human or an agent wrote the code", and routes feature requests to Discussions, which carry no commitment to build anything. There is no path by which this project can cause the capability to exist: we cannot implement it, cannot merge it, and cannot schedule it. That is a structural constraint on the dependency, not a temporary shortage of time.

Everything herdr emits today is ambiguous by construction. `herdr agent list` and `herdr pane get` carry no entry-box field. `herdr pane read` renders the composer line, but in that snapshot text a person typed, a suggestion the agent rendered, and text a tool inserted earlier are indistinguishable. Sampling idle panes showed all three kinds present at once, including scuttlebutt's own prior output.

That last detail is why the obvious local workaround is also out of scope. A gate refusing delivery to any pane whose entry box is non-empty would, in practice, refuse forever on panes holding scuttlebutt's own earlier output — trading a visible paste for silent, permanent starvation. Screen-scraping the composer and diffing it against what scuttlebutt last sent is the only alternative, and it is fragile against a TUI that changes without notice.

## What we do instead

Delivery is gated on herdr's `focused` field, shipped in v0.2.3. A human actively typing at a pane is not pasted into. An unfocused pane holding a half-composed prompt still can be; that exposure is accepted knowingly.

## What this does not cover

This concerns identifying **human** input, which is undecidable from a snapshot. It does not cover matching scuttlebutt's **own** outgoing text, which is unambiguous because scuttlebutt knows byte-for-byte what it sent. Confirming that a batch was submitted is a different problem with a different answer — see #26. Do not cite this file to reject that work.

## Prior requests

- #24 — herdr exposes no trustworthy signal that a pane holds human-typed input
