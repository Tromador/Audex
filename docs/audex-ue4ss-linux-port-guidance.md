# UE4SS File dependency: Linux-port review guidance

## Task and source roles

The work under review is a Linux port of the existing RE-UE4SS `File` dependency.

- **Working implementation (review target):** `/home/stef/paltest/tarballs/RE-UE4SS/deps/first/File`
- **Upstream reference implementation (comparison only):** `/home/stef/projects/RE-UE4SS/deps/first/File`
- **Upstream project:** https://github.com/UE4SS-RE/RE-UE4SS

The reference is a local upstream checkout. Use the supplied files; do not fetch from GitHub or require network access. Compare against the reference where useful to distinguish port-related changes from pre-existing behaviour. Do not treat the reference tree as a separate codebase to audit or report findings against.

## Scope

The developer's primary task is to implement the Linux backend while preserving the established public API and expected behaviour, except where a deliberate platform difference or agreed correction requires otherwise. The Linux implementation should avoid copying known defects from the existing Windows code.

Review for concrete defects in the Linux implementation, relevant shared headers/API/ABI, integration and build configuration necessary for the Linux port, and Windows changes actually made as part of this work.

The original Windows backend is not undergoing a general bug-fixing exercise. Do not report unrelated, pre-existing Windows-only defects as findings against this task. Incidental Windows corrections are in scope only where they are clearly related and straightforward; substantial unrelated fixes belong in separate work.

## Established design context

- `read_all()` on Linux was agreed to support lenient UTF-8 decoding into the API's UTF-16 string: valid UTF-8 should decode correctly; malformed sequences should be replaced with U+FFFD. Automatic detection/handling of UTF-16 files and legacy encoding fallback were explicitly deferred. A failure of the agreed UTF-8 behaviour remains a legitimate defect.
- Windows memory mapping was changed to avoid using `VirtualQuery().RegionSize` as a substitute for file length. An ABI-preserving private mapping-state allocation was agreed, with careful resource ownership and retryable cleanup on unmap failure. Assess actual correctness rather than treating the unusual representation alone as a defect.
- Existing API and binary compatibility matter. Do not propose an incompatible interface change as a routine correction without establishing that the current contract cannot be met safely.

## Evidence and reporting

Find demonstrable defects, not style preferences or requests to implement deferred features. For each finding:

1. Cite the relevant working-tree file and precise line range; cite the upstream reference where it materially establishes provenance.
2. Explain trigger, mechanism, consequence, and whether the defect is introduced by the port, inherited into Linux, or shared with upstream. State when provenance cannot be established.
3. Distinguish a source-supported mechanism from an untested runtime/build outcome; do not assert reproduction without a test.
4. Do not suppress a genuine defect because it touches an accepted compromise. Conversely, do not present an intentional scope limitation as a defect without a concrete adverse consequence within the agreed contract.

An empty findings list is acceptable. The goal is a precise review of the Linux port, not an inventory of every pre-existing issue in the dependency.
