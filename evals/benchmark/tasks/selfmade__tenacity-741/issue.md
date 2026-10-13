A falsy callable wait strategy is never called

`Retrying` skips the wait strategy whenever it is falsy. A wait strategy that is
callable but falsy, for example an object that defines `__bool__` returning False,
or a subclass of an empty container that defines `__call__`, is silently ignored
and the retry sleeps 0 seconds:

```python
import tenacity

class Wait:
    def __bool__(self):
        return False
    def __call__(self, retry_state):
        return 1.5

r = tenacity.Retrying(wait=Wait(), stop=tenacity.stop_after_attempt(2), sleep=lambda s: print("sleep", s))
# the strategy is never called, so it never sleeps 1.5
```

Only `None` and `0` should mean "no wait"; any other strategy must be called, in
both the sync and the asyncio retrying classes.
