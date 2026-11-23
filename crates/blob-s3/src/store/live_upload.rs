//! The completed upload a blob's ordinary reads resolve.

// `Proxiima.Citations.completedUploadOrder`. The query fragment joins blob
// alias `b` to completed-upload alias `u` with its minting identity intact.
macro_rules! with_live_blob_upload {
    ($prefix:literal, $suffix:literal) => {
        concat!(
            $prefix,
            " LATERAL (
                SELECT candidate.*
                  FROM proxiima_core.blob_uploads candidate
                 WHERE candidate.blob_id = b.blob_id
                   AND candidate.owner_id = b.owner_id
                   AND candidate.status = 'completed'
                 ORDER BY candidate.completed_at DESC NULLS LAST,
                          candidate.upload_id DESC
                 LIMIT 1
              ) u ON true ",
            $suffix
        )
    };
}

pub(super) use with_live_blob_upload;
