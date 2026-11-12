# Política de segurança

Detalhes do modelo de ameaças, dos limites de segurança e dos controles pendentes estão em [`docs/SECURITY.md`](docs/SECURITY.md).

Pontos essenciais:

- O código Python dos modelos roda com os privilégios do usuário. **Um venv não é sandbox de sistema operacional.** Execute apenas código confiável.
- Não há sandbox de SO implementado nesta versão.
- Para reportar uma vulnerabilidade, use o canal privado definido pelo responsável pelo projeto. **Canal de contato: pendente de definição** (não publique detalhes em issues, logs ou commits públicos).
