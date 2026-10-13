# Sentinel documentation

Nine documents, each answering one question. Read the left column until you find
your question, then open the file.

## If you are new here

| Your question | Read this |
|---|---|
| What is this, in plain words? | [../README.md](../README.md) "What it does, in plain words" |
| I want to see it running in 5 minutes, with no accounts and no keys | [GETTING-STARTED.md](GETTING-STARTED.md) |
| Why should I believe the numbers? | [HOW-IT-THINKS.md](HOW-IT-THINKS.md) |
| What are the four risk levels and who sets them? | [HOW-IT-THINKS.md](HOW-IT-THINKS.md) and [CONFIGURATION.md](CONFIGURATION.md) |

## If you are installing it

| Your question | Read this |
|---|---|
| What do I need, and in what order? | [SETUP-MANUAL.md](SETUP-MANUAL.md) — the credential set at §0 is the whole checklist |
| Every setting, what it does and where it lives | [CONFIGURATION.md](CONFIGURATION.md) |
| It is running, now what do I do day to day? | [RUNBOOK.md](RUNBOOK.md) |
| Something is broken | [RUNBOOK.md](RUNBOOK.md) §8 "What to do if …" |

## If you are reviewing the engineering

| Your question | Read this |
|---|---|
| How is it built, and what are the invariants? | [architecture.md](architecture.md) |
| What is verified against live systems, and how | [FACTS.md](FACTS.md) |
| What is not finished | [ENTERPRISE-GAPS-STATE.md](ENTERPRISE-GAPS-STATE.md) and [STUBS.md](STUBS.md) |
| The measurements behind every claim | [evidence/](evidence/) — one file per checkpoint |

## If you are judging the results

| Your question | Read this |
|---|---|
| What does the backtest show? | [backtest-report.md](backtest-report.md) (curated suite), [backtest-report-realistic.md](backtest-report-realistic.md) (built from the real venue tape) |
| Which numbers did not work out? | README section "Engineering honesty" — the negative results are published with their mechanism |
| What can I reproduce myself? | Everything. Start with [GETTING-STARTED.md](GETTING-STARTED.md). |

## About the evidence files

`docs/evidence/` holds one file per checkpoint, named for the phase that
produced it (for example `p23-inference-commitment.txt` explains the
proof-carrying AI decisions and lists the exact cases that were run and what
they returned). They are written to be read on their own. When a claim in the
README has an evidence link, that link is the primary source: it contains the
command and its real output, not a summary.

Negative results are in there too, unedited. If a feature was built and made
things worse, its evidence file says so and says why.
