{{ config(materialized='table') }}
{{ def_var('carga_total', false) }}
{% if var('carga_total', false) == true %}
select cliente_id, sum(valor) as total
from {{ source('landing', 'vendas') }}
group by cliente_id
{% else %}
select cliente_id, sum(valor) as total
from {{ source('landing', 'vendas') }}
where valor >= 100
group by cliente_id
{% endif %}
