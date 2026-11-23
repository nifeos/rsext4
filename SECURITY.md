# Security policy

A security report here is about shipped code: a way to make the crate corrupt memory it confines,
spend unbounded resources, or verify a property that does not hold. Report through GitHub's
private vulnerability reporting, which this repository has enabled. What you get back is a real
person reading the report, not a triage queue.

Two facts about this code are worth knowing before you read it. Most of it is upstream's: the
repository is a fork of `rsext4` from rcore-os/tgoskits, and a flaw in code the fork has not
changed is also a flaw upstream. The changes the fork adds were written by AI agents, with one
human reviewing the design and the outcomes.

## Supported versions

One line: the newest release. The shared workflows fail a call at an older template release, and
 Dependabot keeps the crate's own dependencies weekly, so a fix lands as a version bump you can
take.
