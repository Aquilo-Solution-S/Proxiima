//! A spiice writes the projection generator's signature and takes its artifacts
//! apart with names from `proxiima::spiice` alone.

#[test]
fn projection_generator_and_artifact_fields_are_usable_through_the_sdk() {
    use proxiima::spiice::{
        Artifact, ProjectionArtifacts, SpiiceContract, StorageError, projection_artifacts,
    };

    fn generate(contract: &SpiiceContract) -> Result<Option<ProjectionArtifacts>, StorageError> {
        Ok(projection_artifacts(contract)?.map(
            |ProjectionArtifacts {
                 table,
                 index,
                 tag_index,
                 language_trigger,
             }| {
                let Artifact { forward, inverse } = table;
                ProjectionArtifacts {
                    table: Artifact { forward, inverse },
                    index,
                    tag_index,
                    language_trigger,
                }
            },
        ))
    }

    std::hint::black_box(generate);
}
