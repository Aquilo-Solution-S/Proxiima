//! The search-projection half of the contract cross-checks: that a declared
//! projection is internally uniform, and that a query can actually reach it.
//!
//! Split from [`super::contracts`] because these two checks are the only
//! ones that reason about the merged projection statement rather than about
//! one spiice's declaration in isolation — uniformity is a property of what
//! every spiice contributes together, and reachability is a property of the
//! path a query takes to the rows.

use super::{SpiiceRegistry, SpiiceRegistryError};

/// Whether two schemas would hand `ts_rank` different weight arrays.
///
/// `total_cmp` rather than `==`: the values are `f32`s derived from
/// declared relative weights, and a bit-exact total order is both what the
/// renderer's `{}`-formatting reproduces and the only comparison that is
/// meaningful for a float nobody arithmetically combined.
fn weight_arrays_differ(first: Option<[f32; 4]>, second: Option<[f32; 4]>) -> bool {
    match (first, second) {
        (None, None) => false,
        (Some(first), Some(second)) => first
            .iter()
            .zip(second.iter())
            .any(|(a, b)| a.total_cmp(b) != std::cmp::Ordering::Equal),
        _ => true,
    }
}

impl SpiiceRegistry {
    /// The core renderer only merges projections with comparable bands.
    /// Custom SQL describes its own rank source in the contract.
    pub(super) fn validate_projection_is_reachable(
        contract: &crate::spiice::contract::SpiiceContract,
    ) -> Result<(), SpiiceRegistryError> {
        use crate::spiice::contract::BandComparability;

        let Some(spec) = contract.projection.spec() else {
            return Ok(());
        };
        if !spec.rank_source.is_projection() {
            return Ok(());
        }
        for (schema, _) in contract.projected_schemas() {
            if matches!(spec.band_comparability, BandComparability::Divergent { .. }) {
                return Err(SpiiceRegistryError::UnreachableSearchProjection {
                    spiice_id: contract.spiice_id,
                    schema_id: schema.schema_id(),
                    why: "its spiice claims BandComparability::Divergent, and the merge admits \
                          projection-ranked scores only under CoreBands",
                });
            }
        }
        Ok(())
    }

    /// Check the declarations before a renderer reads the first participating schema.
    pub(super) fn validate_contract_projection(
        contract: &crate::spiice::contract::SpiiceContract,
    ) -> Result<(), SpiiceRegistryError> {
        use crate::spiice::contract::{BandComparability, CoreBandName, KeyShape};

        let Some(spec) = contract.projection.spec() else {
            return Ok(());
        };
        let mut reference: Option<&'static crate::spiice::contract::SchemaContract> = None;
        for (schema, _) in contract.projected_schemas() {
            let schema_id = schema.schema_id();
            if matches!(spec.band_comparability, BandComparability::CoreBands) {
                // `Proxiima.Search.admitComparableBand`.
                for band in schema.search.bands() {
                    if !crate::verbs::query::valid_search_score(band.floor)
                        || !crate::verbs::query::valid_search_score(band.ceiling)
                        || band.floor > band.ceiling
                    {
                        return Err(SpiiceRegistryError::ProjectionBandOutsideCoreWindow {
                            spiice_id: contract.spiice_id,
                            schema_id,
                            band: band.name,
                            window: format!("[{}, {}]", band.floor, band.ceiling),
                        });
                    }
                }
            }
            if !spec.rank_source.is_projection() {
                continue;
            }
            // `Proxiima.Search.requiredBands`.
            for name in CoreBandName::ALL {
                if schema.search.band(name.as_str()).is_none() {
                    return Err(SpiiceRegistryError::ProjectionBandName {
                        spiice_id: contract.spiice_id,
                        schema_id,
                        missing: name.as_str(),
                    });
                }
            }
            let Some(first) = reference else {
                reference = Some(schema);
                continue;
            };
            // `Proxiima.Search.uniformProperties`.
            if first.search.language() != schema.search.language() {
                return Err(SpiiceRegistryError::ProjectionRenderNotUniform {
                    spiice_id: contract.spiice_id,
                    schema_id,
                    property: crate::spiice::ProjectionRenderProperty::Language,
                });
            }
            if first.search.bands() != schema.search.bands() {
                return Err(SpiiceRegistryError::ProjectionRenderNotUniform {
                    spiice_id: contract.spiice_id,
                    schema_id,
                    property: crate::spiice::ProjectionRenderProperty::Bands,
                });
            }
            if weight_arrays_differ(
                first.search.rank_weight_array(),
                schema.search.rank_weight_array(),
            ) {
                return Err(SpiiceRegistryError::ProjectionRenderNotUniform {
                    spiice_id: contract.spiice_id,
                    schema_id,
                    property: crate::spiice::ProjectionRenderProperty::RankWeights,
                });
            }
        }
        // A projection row is keyed on the memory its sidecar row belongs
        // to, and the generator reads that column off the sidecar's
        // SURFACE — the one declaration that says how the table is keyed.
        // A projected schema whose sidecar declares no surface, or one
        // keyed on anything but the memory `t`, therefore has no statement
        // to generate. Refused here rather than left to the generator,
        // because there it is found twice and late: once on the write side
        // when the DDL is emitted at boot, and again on the read side at
        // the first query that expects rows.
        //
        // Read off `all_surfaces()` rather than the schema's own list: a
        // spiice is free to declare its sidecar among the spiice's state
        // surfaces, and where the declaration sits does not change which
        // column the generator reads.
        for (schema, sidecar_table) in contract.projected_schemas() {
            let keyed_on_memory = contract.all_surfaces().any(|surface| {
                surface.table == sidecar_table && matches!(surface.key, KeyShape::MemoryT { .. })
            });
            if !keyed_on_memory {
                return Err(SpiiceRegistryError::ProjectedSidecarNotMemoryKeyed {
                    spiice_id: contract.spiice_id,
                    schema_id: schema.schema_id(),
                    table: sidecar_table,
                });
            }
        }
        Ok(())
    }
}
