select g::int4 as v
from generate_series(1, 3) as g
{{ run("transforma.py") }}
