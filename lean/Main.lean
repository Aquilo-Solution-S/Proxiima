import TeslaValve

/-- Checks, draws and emits the ledger, beside the Causa proof library. -/
unsafe def main (args : List String) : IO UInt32 :=
  TeslaValve.run { ledger := `Proxiima, proofs := [`Causa] } args
