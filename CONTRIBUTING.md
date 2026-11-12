# Contribuindo com o DOWAI

Obrigado pelo interesse. Este guia descreve como preparar, testar e enviar
alterações ao projeto.

## Pré-requisitos

- Rust stable com `rustfmt` e `clippy` (`rustup component add rustfmt clippy`).
- Python 3.10 ou superior para o SDK em `src/dowai-sdk`.
- PostgreSQL 16 apenas para os testes marcados como `#[ignore]`, executados com
  as variáveis `DOWAI_TEST_PG_*` (ver `docs/STATUS.md`).
- No Linux, as bibliotecas de desenvolvimento do OpenSSL (`libssl-dev`) são
  necessárias para compilar as dependências nativas.

## Fluxo de trabalho

1. Crie uma branch a partir da branch principal.
2. Faça alterações pequenas e focadas em um único objetivo.
3. Adicione ou atualize testes para o comportamento alterado.
4. Atualize `CHANGELOG.md` na seção `Unreleased` quando a mudança for visível
   ao usuário, à CLI, ao SDK, aos formatos JSON ou às configurações.
5. Envie um pull request descrevendo o problema, a solução e as evidências de
   teste executadas.

Commits devem ter mensagem descritiva no formato `tipo(escopo): resumo`, por
exemplo `feat(cli): adicionar dowai retry`.

## Validação antes do envio

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Para o SDK Python:

```bash
cd src/dowai-sdk
PYTHONPATH=src python3 -m unittest discover -s tests -p "test_*.py"
```

Testes com PostgreSQL devem ser executados com `--ignored --test-threads=1`
quando o cluster de teste estiver disponível. Não declare um teste como
executado se ele não rodou; registre a limitação no pull request.

## Padrões de código

- Siga as convenções existentes de cada crate e do SDK.
- Não inclua segredos, senhas, tokens ou dados reais em código, testes,
  fixtures ou logs.
- Não interpole valores de variáveis em SQL; use apenas os mecanismos de
  template previstos em `docs/` e no `plano_mestre.md`.
- Mudanças em formatos JSON, códigos de erro ou exit codes devem atualizar os
  schemas em `specs/` e a documentação correspondente.

## Licença

Ao contribuir, você concorda que sua contribuição será distribuída sob a
licença Apache-2.0 do projeto (ver `LICENSE`).

## Segurança

Não abra issues públicas para vulnerabilidades. Siga o procedimento descrito
em `SECURITY.md`.
