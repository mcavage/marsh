Deleting a slice from an Array corrupts its rendered TOML

After deleting a slice from a `tomlkit` array, the list contents are right but
`as_string()` no longer matches them:

```pycon
>>> from tomlkit import item
>>> a = item([1, 2, 3])
>>> del a[:0]          # deletes nothing
>>> a
[1, 2, 3]
>>> a.as_string()      # expected "[1, 2, 3]"
'[]'
```

Slices with a negative start or stop (`del a[-1:]`, `del a[:-1]`) also leave the
rendered output out of sync with the contents. Rendering should always agree with
the list after any slice deletion.
