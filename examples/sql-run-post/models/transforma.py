# Recebe `origem` do SQL que chamou com run(). Quando executado diretamente
# (`dowai run transforma.py`) não há pai: origem é None e o modelo deve tratar isso.
def model(env, origem=None):
    if origem is None:
        env.setVar("origem_recebida", False)
        env.setVar("linhas_origem", 0)
        return None
    total = sum(len(batch) for batch in origem.iter_batches(500))
    env.setVar("origem_recebida", True)
    env.setVar("linhas_origem", total)
    return None
