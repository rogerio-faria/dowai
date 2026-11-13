{{ config(materialized='table') }}
select c.id as cliente_id,
       c.nome,
       sum(v.valor) as total
from {{ ref('stg_clientes') }} as c
join {{ source('landing', 'vendas') }} as v on v.cliente_id = c.id
group by c.id, c.nome
