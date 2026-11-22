-- `Proxiima.Memory.confidenceMax`.
-- Enforce writes immediately while allowing upgrades with legacy invalid scores.
ALTER TABLE proxiima_core.interpretation_v1
    ADD CONSTRAINT interpretation_v1_confidence_chk
    CHECK (confidence BETWEEN 0 AND 100) NOT VALID;
