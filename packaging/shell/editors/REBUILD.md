# Terminal Emacs source build

The `emacs` stage of the accompanying Dockerfile downloads checksum-pinned
Debian source `1:30.1+1-6+deb13u1`, applies these runtime backports with zero
fuzz, and runs autogen/configure/make/make install under `/opt/emacs`:

- Flymake CVE-2026-96442: the minimal safe patch published at
  https://www.openwall.com/lists/oss-security/2026/09/14/1
- TRAMP CVE-2026-79992: upstream commit
  `f3e7104d05bdb8e32ba13bf75604108ad88536dc`
- Complete TRAMP fix (including CVE-2026-108976): upstream commit
  `044b0203267db3b7912325d076d79a79000afe75`

The TRAMP patches are backported to Emacs 30. Native compilation and X are
disabled. `SHA256SUMS` lists the source archives and applied patches; all
accompany the binary.

From the matching marsh checkout, run
`python3 scripts/prepare-shell-image.py --validate-only` with Docker/DHI access.
The base input is `packaging/shell-image`.
