# subbier — principles

Three rules. A change that breaks one of them is wrong even if it works.

## The library is the product. Frontends are projections.

`libsubby` owns state, polling, the proxy, balancing and the presentation
primitives, and has zero UI dependencies. A frontend reads a `Snapshot` and
sends a `Command`. It never computes a percentage, ranks an account, or decides
what a bar means.

Adding a frontend is drawing a `Snapshot`. That is why the menu, the CLI and the
TUI agree without knowing about each other. The converse holds too: platform
side effects stay out of the library.

## Built for a glance, at any count.

subbier is not an investigative tool. Its job is to answer "how am I doing?"
in the time it takes to look at the menu bar. Every surface is designed
assuming anywhere from one sub to dozens, and has to read at a glance across
that whole range. If a layout only works at three accounts, it does not work.

Density follows from this. It is a constraint, not a preference: anything that
scales per-sub has to stay small enough that a dozen of them still fit in one
look. Detail that would not survive that test belongs in a subcommand, not on
the readout.

## Simple.

Every concept in subbier needs a mental model that fits in a sentence. Chains,
pools, quarantine, allowance versus proxied tokens: if one cannot be explained
to a new user without a diagram, it is the wrong concept, not a documentation
gap.

No magic. subbier does what it says and nothing it does not say: it does not
rewrite requests, invent numbers it did not observe, or take over files it only
needed to read. When a behaviour has to exist for the thing to work at all, it
is written down where the user will see it.
