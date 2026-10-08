"""Bounded canonical Dockerfile instruction reader, NOT a general Docker parser.

Only the default backslash escape and the syntax directive are admitted. Reject
other parser directives before joining lines, even if Docker would ignore a late
one. A reviewed source fence never waives this grammar restriction.
"""
import re


def recipe_instructions(text):
    for line in text.splitlines():
        directive = re.match(r'^\s*#\s*([A-Za-z][A-Za-z0-9_-]*)\s*=', line)
        if directive and directive[1].lower() != 'syntax':
            raise ValueError('unsupported Dockerfile parser directive: ' + directive[1])
    # Current canonical recipes have no heredocs. Refuse rather than treating
    # heredoc contents as Dockerfile instructions or overlooking later effects.
    if any('<<' in line for line in text.splitlines() if not line.lstrip().startswith('#')):
        raise ValueError('unsupported Dockerfile heredoc grammar')
    result, pending = [], ''
    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith('#'):
            continue
        if line.endswith('\\\\'):
            raise ValueError('unsupported repeated Dockerfile continuation escape')
        if line.endswith('\\'):
            pending += line[:-1] + ' '
            continue
        line = pending + line
        pending = ''
        instruction = re.fullmatch(r'([A-Za-z]+)[ \t]+(.+)', line)
        if not instruction:
            raise ValueError('unsupported Dockerfile instruction grammar')
        result.append(instruction[1].upper() + ' ' + instruction[2])
    if pending:
        raise ValueError('unterminated Dockerfile continuation')
    return result
