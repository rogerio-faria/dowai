from dowai import DowaiExecutionError


def model(env):
    env.setVar("carga_total", True)
    with env.manual():
        env.run("arq1.sql")
        try:
            env.run("arq10.sql")
        except DowaiExecutionError:
            # Falha tolerada por decisão do modelo: o restante da sequência continua.
            env.setVar("falha_tolerada", True)
        env.run("arq7.py")
    return None
