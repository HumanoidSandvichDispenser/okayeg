# Collaboration

Beyond editing the text directly, Okayeg gives collaborators two ways to work
on a doc: annotations, for talking about a span of text, and suggestions, for
offering a change to it.

## Annotations

An annotation is a note attached to a range of the doc. It lives in its own
container inside the same doc as the text it refers to, and it anchors to that
text through a Loro cursor, so the note stays on the right span as edits shift
the surrounding content around.

Because annotations are their own container, access to them is decided the same
way as access to any other resource in the doc. A relay inspects which
containers a change touches, so it can accept edits to the annotation container
from someone who may comment while still refusing their edits to the prose (see
[Access control](access-control.md)). Commenting and writing are separate
grants over the same doc.

## Suggestions

A suggestion is the [Submit](sync.md#push-submit-hold) path. With propose
access, an edit you make applies to your own copy and then goes upstream as a
proposal that someone with write access reviews. The edits accumulate in your
fork and are offered together as a bundle, reviewed as a unit, and the reviewer
accepts or denies it.

What's specific to a suggestion is how it reads before that decision. A
proposal carries the ops of your edit against the version you branched from, so
a reviewer sees the suggested change in place against the current text, the
insertions and deletions it makes, and decides on the whole bundle. Accepting
merges those ops into the shared doc, where they become ordinary history.
Denying drops the bundle, and your fork keeps the change as long as you want it.
