import TeslaValve

/-- Checks, draws and emits the ledger, beside the Causa proof library. -/
unsafe def main (args : List String) : IO UInt32 :=
  TeslaValve.run {
    ledger := `Proxiima
    proofs := [`Causa]
    scopes := [{ rules := `Proxiima, code := [".", ":!lean", ":!docs", ":!*.md",
      ":!llms*.txt", ":!crates/core/tests/ledger_vectors.json"] }]
  } args
