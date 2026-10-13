# Evaluations

Three evaluations of marsh as a user meets it. Each has a design to read first,
scaffolding to run, and a dry run that needs no VM, credentials, or model.
The benchmark has validated tasks and a real-agent calibration; none has had its full run yet.

| Evaluation | Question | Design | Estimated cost |
|---|---|---|---|
| Benchmark pilot | Do race, retry, and judge workflows fix more real bugs, scored on hidden tests? | [benchmark/DESIGN.md](benchmark/DESIGN.md) | ≈ $15 expected (calibrated), $90 cap |
| Agent reach | Can a model with only llms.txt install and use marsh correctly? | [reach/DESIGN.md](reach/DESIGN.md) | ≈ $32, $40 stop |
| Recorded sessions | Where do five developers new to marsh stall, and what do they believe it does? | [recording/SCRIPT.md](recording/SCRIPT.md) | ≈ $100 in capped keys, plus compensation |

`common/fakemarsh.py` stands in for the `marsh` CLI and its agents in dry
runs. It implements the host-side `split`/`join`, `fanout`/`collect`, and
`-c`, with scripted agents. It is for testing the harnesses only; never report
a score from it.

Hermetic checks (part of `make test-harness`):

```sh
python3 -W error -m unittest discover -s evals/benchmark -p 'test_*.py'
python3 -W error -m unittest discover -s evals/reach -p 'test_*.py'
```

The designs were reviewed twice by an outside model acting as a skeptical
methodology reviewer. Each design's limitations section records the points
that remain open.
