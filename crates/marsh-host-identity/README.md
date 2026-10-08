# Descriptor-bound host filesystem identity

`read(&File)` is a read-only safe Rust API over macOS `fgetattrlist`. The caller
keeps the file open. No pathname, permission mutation, ACL setter, subprocess,
FD transfer, or fallback identity is involved.

The native call is isolated in this small crate because the main adapter forbids
unsafe Rust. `unsafe_code` is denied here except at the one synchronous syscall
wrapper. It passes initialized request/output buffers with exact lengths and
keeps the descriptor alive. Returned bytes are decoded without unaligned typed
loads or dereferencing returned pointers.

Two separate requests are necessary:

- `ATTR_VOL_INFO | ATTR_VOL_CAPABILITIES | ATTR_VOL_UUID`: 52 packed bytes,
  including length, four capability words, four validity words and the UUID.
- `ATTR_CMN_FILEID`: 12 packed bytes, including length and a 64-bit ID.

Unexpected lengths, absent validity/support bits, zero UUID/ID and syscall errors
fail closed. Both `VOL_CAP_FMT_PATH_FROM_ID` and `VOL_CAP_FMT_64BIT_OBJECT_IDS`
are required. Apple specifies that the former implies persistent **non-recycled**
object IDs, and that the latter selects `ATTR_CMN_FILEID` rather than legacy
32-bit attributes. Width or volume UUID alone would not establish incarnation.
Birthtime, write-generation counters and `st_gen` are not substituted.

Primary definitions: Apple's [sys/attr.h](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/sys/attr.h)
and [getattrlist manual](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/man/man2/getattrlist.2).
The security evidence packet preserves the exact retrieved bytes/hashes and the
independent installed Apple SDK/native APFS receipts. These links are references,
not runtime build inputs. The supported product host remains ARM64 macOS.

Non-macOS returns `Unsupported`; decoder tests on Linux only test packed-byte
validation. The adapter's explicitly marked recording-fixture identities are not
provided by this crate and are not APFS evidence. The native descriptor test must
run on an actual supported Mac volume. A trusted host cloning/reformatting a
volume is outside the identity premise.

This fixes historical inode-incarnation comparisons, not stock's pathname reopen
race, grant revocation/drain, or VM destruction closure. It is not a mount API.
