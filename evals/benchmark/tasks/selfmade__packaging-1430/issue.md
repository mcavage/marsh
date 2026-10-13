Empty arbitrary-equality version (`===`) is accepted

The arbitrary equality operator `===` requires a version, but an empty one is
accepted everywhere:

```pycon
>>> from packaging.specifiers import Specifier, SpecifierSet
>>> from packaging.requirements import Requirement
>>> Specifier("===")
<Specifier('===')>
>>> SpecifierSet(">=1,===")
<SpecifierSet('===,>=1')>
>>> Requirement("foo===")
<Requirement('foo===')>
```

Each of these should be rejected (`InvalidSpecifier`, or `InvalidRequirement` with a
message that mentions `===`). Non-empty arbitrary versions such as `foo===bar===`
must keep working.
