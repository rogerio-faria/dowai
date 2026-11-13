def model(env):
    with env.manual():
        env.run("arq1.sql")
        env.run("arq10.sql")
        # Sem try: a falha de arq10 interrompe o modelo, e arq7.py não é iniciado.
        env.run("arq7.py")
    return None
