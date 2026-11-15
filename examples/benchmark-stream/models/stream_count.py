"""ENT-076 workload: writes and re-reads a CSV of `N` rows in streaming.

The CSV is about 25 bytes per row (about 150 MB for N = 6 000 000), larger than the
`runtime.max_memory_mb` of 128 MiB in dowai_project.json. The model never holds the rows in
memory: `env.generateCsv` streams them and the read-back is line by line.

Set `python.interpreter` in dowai_project.json to the project's virtual environment before running
(`python3 -m venv .venv`). See docs/PERFORMANCE.md for the procedure and the measured results.
"""

import csv

N = 6000000
PATH = "bench/stream.csv"


def rows(n):
    for i in range(n):
        yield {"id": i, "cliente": "cliente-%d" % i, "valor": (i % 1000) / 10}


def model(env):
    written = env.generateCsv(rows(N), PATH)
    total = 0.0
    count = 0
    with open(PATH, newline="") as handle:
        for record in csv.DictReader(handle):
            total += float(record["valor"])
            count += 1
    env.setVar("rows_written", written)
    env.setVar("rows_read", count)
    env.setVar("valor_total", round(total, 1))
    return None
