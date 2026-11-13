def model(env):
    env.depends_on("faturamento")
    df = env.ref("faturamento").to_pandas()
    df["total"] = df["total"].astype("float64")
    df["comissao"] = (df["total"] * 0.05).round(2)
    env.write(df, target="financeiro", table="comissoes", mode="append")
    env.setVar("linhas_comissao", int(len(df)))
    return None
