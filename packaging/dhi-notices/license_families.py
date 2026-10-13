"""License families for the license-delta report: a coarse, conservative label.

A family is what a review decides on (MIT, Apache-2.0, GPL, Proprietary, ...),
not an SPDX expression. Texts are matched by fingerprints of their operative
wording; declarations (npm `license`, Debian DEP-5 `License:`, SPDX
expressions) by name. Notices, author lists and other non-license files match
nothing and add no family. This labels evidence for a person; it is not a
license determination.
"""

import re

# (family, fingerprint of the license's own wording, lowercased, whitespace folded)
TEXT_FINGERPRINTS = (
    ("Proprietary", r"all rights reserved\. use is subject to|\bproprietary\b and confidential"),
    ("Apache-2.0", r"apache license,? version 2\.0|apache license\s+version 2\.0"),
    ("MIT", r"permission is hereby granted, free of charge"),
    ("BSD", r"redistribution and use in source and binary forms"),
    ("ISC", r"permission to use, copy, modify, and(/or)? distribute this software for any purpose"),
    ("MPL-2.0", r"mozilla public license"),
    ("LGPL", r"gnu (lesser|library) general public license"),
    ("GPL", r"gnu general public license"),
    ("Zlib", r"altered source versions must be plainly marked"),
    ("Unlicense", r"this is free and unencumbered software"),
    ("CC0-1.0", r"\bcc0 1\.0\b|creative commons legal code\s+cc0"),
    ("CC-BY-SA", r"attribution-sharealike"),
    ("BSL-1.0", r"boost software license"),
    ("Unicode", r"unicode, inc\.|unicode license"),
    ("Python-2.0", r"python software foundation license|\bpsf license\b"),
    ("OpenSSL", r"openssl license|developed by the openssl project"),
    ("Artistic", r"artistic license"),
    ("0BSD", r"zero-clause bsd"),
    ("CDLA", r"community data license agreement"),
    ("BlueOak", r"blue oak model license"),
)

# Declared identifiers (SPDX ids, DEP-5 short names, npm strings) to families.
DECLARED = (
    ("Proprietary", r"^(see license in|unlicensed$|proprietary|commercial|non-free)"),
    ("Apache-2.0", r"^apache"),
    ("MIT", r"^(mit|expat|x11|mit-0)\b"),
    ("BSD", r"^bsd"),
    ("0BSD", r"^0bsd"),
    ("ISC", r"^isc"),
    ("MPL-2.0", r"^mpl"),
    ("LGPL", r"^lgpl"),
    ("GPL", r"^(gpl|agpl)"),
    ("Zlib", r"^zlib"),
    ("Unlicense", r"^unlicense$"),
    ("CC0-1.0", r"^cc0"),
    ("CC-BY-SA", r"^cc-by-sa"),
    ("CC-BY", r"^cc-by(-[0-9]|$)"),
    ("BSL-1.0", r"^bsl-1\.0"),
    ("Unicode", r"^unicode"),
    ("Python-2.0", r"^(python|psf)"),
    ("OpenSSL", r"^openssl"),
    ("Artistic", r"^artistic"),
    ("BlueOak", r"^blueoak"),
    ("Public-Domain", r"^public[- ]domain"),
    ("WTFPL", r"^wtfpl"),
    ("curl", r"^curl"),
)


def _fold(data):
    text = data.decode("utf-8", "replace") if isinstance(data, bytes) else data
    return re.sub(r"\s+", " ", text.lower())


def text_families(data):
    text = _fold(data)
    return {name for name, pattern in TEXT_FINGERPRINTS if re.search(pattern, text)}


def declared_families(expression):
    """Families of an SPDX-like expression or a DEP-5/npm license name."""
    if not expression:
        return set()
    result = set()
    for term in re.split(r"\s+(?:and|or|with)\s+|[(),/|&]", expression.strip(), flags=re.I):
        term = term.strip().lower()
        if not term or term.endswith("exception"):
            continue
        matched = {name for name, pattern in DECLARED if re.search(pattern, term)}
        result |= matched or {"Unknown:" + term}
    return result


def dep5_families(data):
    """Families declared by a machine-readable Debian copyright file, or None."""
    text = data.decode("utf-8", "replace")
    if not re.match(r"format:\s*https?://(www\.)?debian\.org/doc/packaging-manuals/copyright-format/",
                    text, re.I):
        return None
    result = set()
    for match in re.finditer(r"^License:[ \t]*(\S.*)$", text, re.M | re.I):
        result |= declared_families(match.group(1))
    return result


def copyright_families(data):
    families = dep5_families(data)
    return families if families else text_families(data)
