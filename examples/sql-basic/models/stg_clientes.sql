{{ config(materialized='table') }}
select id, nome
from {{ source('landing', 'clientes') }}
