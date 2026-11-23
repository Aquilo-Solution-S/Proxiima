/-! # Search -/

namespace Proxiima.Search

/-- The shared ceiling in characters for tool text and hydrated search bodies. -/
def textMaxChars : Nat := 8000

/-- The default requested search page size. -/
def defaultPageSize : Nat := 8

/-- The maximum number of hits in a Memory search page. -/
def pageSizeMax : Nat := 50

/-- The maximum number of search space selectors before deduplication. -/
def spacesMax : Nat := 16

/-- The maximum number of raw search tag filters. -/
def tagsMax : Nat := 16

/-- The maximum consumed depth of a relevance cursor. -/
def relevanceDepthMax : Nat := 5000

/-- The lower endpoint of the search score interval. -/
def unitScoreFloor : Nat := 0

/-- The upper endpoint of the search score interval. -/
def unitScoreCeiling : Nat := 1

/-- The denominator offset for federated reciprocal ranks. -/
def storeRankOffset : Nat := 60

/-- The denominator offset for hybrid reciprocal ranks. -/
def hybridRankOffset : Nat := 60

/-- The default semantic weight on a ten-point scale. -/
def hybridSemanticWeightTenths : Nat := 5

/-- The fixed candidate horizon per hybrid leg. -/
def hybridWindow : Nat := 1000

/-- The core projection candidate limit per statement. -/
def coreProjectionCap : Nat := 1000

/-- The non-hybrid budget multiplier before the projection cap. -/
def requestOverfetchFactor : Nat := 20

/-- The per-query cap for scanning semantic candidates. -/
def semanticScanCap : Nat := 1000

/-- The snippet budget in bytes. -/
def snippetBytes : Nat := 480

/-- The deadline in seconds on a vector index request. -/
def vectorRequestSeconds : Nat := 10

/-- The byte limit on a vector index request body. -/
def vectorBodyBytes : Nat := 33554432

/-- The minimum character count for a vector index token. -/
def vectorTokenMinChars : Nat := 32

/-- The explicit automatic lexical configuration request. -/
def lexicalAuto : String := "auto"

/-- The explicit deployment lexical configuration sentinel. -/
def lexicalDeploymentDefault : String := "@deployment"

/-- PostgreSQL text search weight classes in increasing weight order. -/
def weightClasses : List String := ["D", "C", "B", "A"]

/-- Default PostgreSQL weights by increasing class on a ten-point scale. -/
def postgresWeightTenths : List Nat := [1, 2, 4, 10]

/-- The declared relative weight of a uniformly weighted field. -/
def uniformWeight : Nat := 1

/-- The flag combination omitting normalization. -/
def normalizationNone : Nat := 0

/-- The PostgreSQL rank scaling flag. -/
def normalizationScale : Nat := 32

/-- The PostgreSQL logarithmic document length flag. -/
def normalizationLogLength : Nat := 1

/-- The decimal precision of rendered band endpoints. -/
def bandDecimalPlaces : Nat := 2

/-- The exact match band floor as a percentage. -/
def exactFloorPercent : Nat := 50

/-- The exact match band ceiling as a percentage. -/
def exactCeilingPercent : Nat := 100

/-- The rescue band floor as a percentage. -/
def rescueFloorPercent : Nat := 25

/-- The rescue band ceiling as a percentage. -/
def rescueCeilingPercent : Nat := 45

/-- The flat substring score as a percentage. -/
def substringPercent : Nat := 25

/-- The minimum admitted HNSW search breadth. -/
def hnswEfSearchMin : Nat := 1

/-- The maximum admitted HNSW search breadth. -/
def hnswEfSearchMax : Nat := 1000

/-- The minimum admitted HNSW iterative scan tuple cap. -/
def hnswScanTuplesMin : Nat := 1

/-- The maximum admitted HNSW iterative scan tuple cap. -/
def hnswScanTuplesMax : Nat := 2147483647

/-- What a candidate source can conclude about the search window. -/
inductive CandidateBound where
  | complete
  | bounded
  | unknown
  deriving DecidableEq

/-- Any known cap dominates, and exhaustion needs confirmation from every leg. -/
def combineBounds : CandidateBound → CandidateBound → CandidateBound
  | .complete, .complete => .complete
  | .bounded, .complete | .bounded, .bounded | .bounded, .unknown
  | .complete, .bounded | .unknown, .bounded => .bounded
  | .complete, .unknown | .unknown, .complete | .unknown, .unknown => .unknown

/-- Comparison outcomes for a pair of ranked hits. -/
inductive RankComparison where
  | lt
  | eq
  | gt
  deriving DecidableEq

/-- Compare federated scores first, then list position, then source rank. -/
def storeHitOrder : RankComparison → RankComparison → RankComparison → RankComparison
  | .lt, .lt, .lt
  | .lt, .lt, .eq
  | .lt, .lt, .gt
  | .lt, .eq, .lt
  | .lt, .eq, .eq
  | .lt, .eq, .gt
  | .lt, .gt, .lt
  | .lt, .gt, .eq
  | .lt, .gt, .gt
  | .eq, .lt, .lt
  | .eq, .lt, .eq
  | .eq, .lt, .gt
  | .eq, .eq, .lt => .lt
  | .eq, .eq, .eq => .eq
  | .eq, .eq, .gt
  | .eq, .gt, .lt
  | .eq, .gt, .eq
  | .eq, .gt, .gt
  | .gt, .lt, .lt
  | .gt, .lt, .eq
  | .gt, .lt, .gt
  | .gt, .eq, .lt
  | .gt, .eq, .eq
  | .gt, .eq, .gt
  | .gt, .gt, .lt
  | .gt, .gt, .eq
  | .gt, .gt, .gt => .gt

/-- Keep a distinct edge when at least one endpoint survives the merged page. -/
def neighborEdgeEligible : (sourceSurvives : Bool) → (targetSurvives : Bool) → (duplicate : Bool) → Bool
  | true, true, false | true, false, false | false, true, false => true
  | true, true, true | true, false, true | false, true, true
  | false, false, true | false, false, false => false

/-- The requested ranking mode and the effective mode on an owner lane. -/
inductive SearchMode where
  | lexical
  | semantic
  | hybrid
  deriving DecidableEq

/-- Search proceeds on a ready lane, or degrades or refuses an unavailable semantic leg. -/
inductive LaneRoute where
  | lexical
  | semantic
  | hybrid
  | degradedLexical
  | refuse
  deriving DecidableEq

/-- Hybrid falls back to lexical, while explicitly semantic search requires a ready vector leg. -/
def laneRoute : SearchMode → (semanticReady : Bool) → LaneRoute
  | .lexical, true | .lexical, false => .lexical
  | .semantic, true => .semantic
  | .semantic, false => .refuse
  | .hybrid, true => .hybrid
  | .hybrid, false => .degradedLexical

/-- A supplied fusion weight follows only an effective hybrid search. -/
def keepSemanticWeight : SearchMode → (supplied : Bool) → Bool
  | .hybrid, true => true
  | .hybrid, false | .lexical, true | .lexical, false
  | .semantic, true | .semantic, false => false

/-- The order of choosing candidates and presenting their snippet on a result page. -/
inductive CandidateStage where
  | restrictToAdmissible
  | boundRankedWindow
  | admitVisibleMemory
  | selectPage
  | hydrateSnippet
  deriving DecidableEq

/-- Spend the window on admissible candidates, read visible Memories, then hydrate the selected page. -/
def candidateStages : List CandidateStage :=
  [.restrictToAdmissible, .boundRankedWindow, .admitVisibleMemory, .selectPage, .hydrateSnippet]

/-- The fields determining whether a ranked projection participates in the shared search. -/
inductive ProjectionRequirement where
  | rendererBands
  | comparableScale
  | matchesRequestedKind
  | matchesRequestedSchema
  | supportsRequestedTags
  deriving DecidableEq

/-- An unscoped search still requires the renderer's comparable bands and narrows only by supplied filters. -/
def projectionRequirements : List ProjectionRequirement :=
  [.comparableScale, .rendererBands, .matchesRequestedKind, .matchesRequestedSchema, .supportsRequestedTags]

/-- The properties a shared lexical statement renders once for its spiice. -/
inductive RenderProperty where
  | language
  | bands
  | rankWeights
  deriving DecidableEq

/-- All projected schemas sharing the renderer agree on language, score windows and rank weights. -/
def uniformProperties : List RenderProperty := [.language, .bands, .rankWeights]

/-- Rank windows known to the shared projection renderer. -/
inductive CoreBand where
  | exact
  | rescue
  | substring
  deriving DecidableEq

/-- The shared renderer resolves exact, rescue and substring windows by name. -/
def requiredBands : List CoreBand := [.exact, .rescue, .substring]

/-- Lexical configuration policy for a projected row. -/
inductive LanguagePolicy where
  | perRow
  | pinned
  | pinnedUnion
  deriving DecidableEq

/-- Which configuration a lexical projection stamps. -/
inductive LanguageStamp where
  | rowConfig
  | pinnedConfig
  | firstPinnedConfig
  deriving DecidableEq

/-- Use the row's language, the pinned configuration, or the first member of a pinned union. -/
def languageStamp : LanguagePolicy → LanguageStamp
  | .perRow => .rowConfig
  | .pinned => .pinnedConfig
  | .pinnedUnion => .firstPinnedConfig

/-- A ready substring leg is used only when the schema opted in and returned no ranked hits. -/
def usesSubstring : (declared : Bool) → (rankedHit : Bool) → Bool
  | true, false => true
  | true, true | false, true | false, false => false

/-- The evidence that tells relevance paging how to merge the selected lanes. -/
inductive RelevanceLanes where
  | single
  | equalScale
  | unequalScale
  deriving DecidableEq

/-- The ordering space that also defines the continuation. -/
inductive RelevanceOrder where
  | score
  | perLaneRank
  deriving DecidableEq

/-- Compare scores on one scale and interleave ranks across unequal scales. -/
def relevanceOrder : RelevanceLanes → RelevanceOrder
  | .single | .equalScale => .score
  | .unequalScale => .perLaneRank

/-- Ordered keys carried by a relevance continuation. -/
inductive RelevanceKey where
  | scoreBits
  | admissionId
  | consumedDepth
  deriving DecidableEq

/-- Preserve the last emitted score exactly with its admission ID and consumed depth. -/
def relevanceCursorKeys : List RelevanceKey := [.scoreBits, .admissionId, .consumedDepth]

/-- The fields used to advance the index's entry scan. -/
inductive VectorKeyField where
  | entityId
  | model
  | dimension
  deriving DecidableEq

/-- Page vector entries strictly after entity ID, model and dimension. -/
def vectorEntryKeys : List VectorKeyField := [.entityId, .model, .dimension]

/-- A request that reaches the vector HTTP boundary. -/
inductive VectorOperation where
  | put
  | delete
  | owner
  | nearest
  | entries
  deriving DecidableEq

/-- The typed POST operations exposed by the vector index router. -/
def vectorOperations : List VectorOperation := [.put, .delete, .owner, .nearest, .entries]

/-- The branch that prices an index HTTP result. -/
inductive VectorHttpResult where
  | refused
  | unavailable
  | unknownStore
  deriving DecidableEq

/-- Use distinct response statuses for refusal, unavailability and unknown stores. -/
def vectorErrorStatus : VectorHttpResult → Nat
  | .refused => 400
  | .unavailable => 503
  | .unknownStore => 404


/-- Default ranking mode when the search request omits it. -/
def defaultMode : SearchMode := .hybrid

/-- Warn a spiice caller when a nonempty hybrid page has no positive semantic score. -/
def warnsOnLexicalDegradation : SearchMode → (noRows : Bool) → (hasPositiveSemanticScore : Bool) → Bool
  | .hybrid, false, false => true
  | .hybrid, true, true | .hybrid, true, false | .hybrid, false, true
  | .lexical, true, true | .lexical, true, false | .lexical, false, true | .lexical, false, false
  | .semantic, true, true | .semantic, true, false | .semantic, false, true | .semantic, false, false => false

/-- Search tag processing stages. -/
inductive TagFilterStage where
  | capRawList
  | trim
  | foldAscii
  | discardBlank
  | sort
  | deduplicate
  deriving DecidableEq

/-- Bound raw filters before normalizing them to distinct nonblank exact tags. -/
def tagFilterStages : List TagFilterStage := [.capRawList, .trim, .foldAscii, .discardBlank, .sort, .deduplicate]

/-- The steps that turn admitted hybrid hits into a page. -/
inductive HybridStage where
  | floorEachLeg
  | rankEachLeg
  | sumWeightedReciprocals
  | pageFusedResults
  deriving DecidableEq

/-- Apply each leg's raw floor before one-based reciprocal ranking and then page the weighted sum. -/
def hybridStages : List HybridStage := [.floorEachLeg, .rankEachLeg, .sumWeightedReciprocals, .pageFusedResults]

/-- Ranked search can retain a fixed horizon, widen with cursor depth or push the keyset into the candidate scan. -/
inductive CandidateHorizon where
  | fixed
  | depthPlusPage
  | recencyKeyset
  deriving DecidableEq

/-- Hybrid fixes the horizon in both orders; pure relevance widens within the cap and pure recency applies its keyset. -/
def candidateHorizon : (hybridFusion : Bool) → (recency : Bool) → CandidateHorizon
  | true, true | true, false => .fixed
  | false, true => .recencyKeyset
  | false, false => .depthPlusPage

/-- The lexical forms of pgvector's iterative index scan. -/
inductive IterativeScan where
  | off
  | strictOrder
  | relaxedOrder
  deriving DecidableEq

/-- The GUCs that a semantic search sets locally for its transaction. -/
inductive HnswSetting where
  | searchBreadth
  | iterativeScan
  | scanTupleCap
  deriving DecidableEq

/-- Enabled iterative scans set the tuple cap in addition to the scan mode and breadth. -/
def hnswSettings : IterativeScan → List HnswSetting
  | .off => [.searchBreadth, .iterativeScan]
  | .strictOrder | .relaxedOrder => [.searchBreadth, .iterativeScan, .scanTupleCap]

/-- Default HNSW search breadth. -/
def hnswDefaultBreadth : Nat := 100

/-- Default tuple limit for iterative HNSW scans. -/
def hnswDefaultTupleCap : Nat := 20000

/-- Default mode for iterative HNSW scans. -/
def hnswDefaultMode : String := "relaxed_order"

/-- Index bounds distinguish a capped chunk scan from a proof of Memory exhaustion. -/
def chunkCandidateBound : (empty : Bool) → (reachedCap : Bool) → CandidateBound
  | true, true | true, false => .complete
  | false, true => .bounded
  | false, false => .unknown

/-- The HTTP response carries hits without the source's completeness claim. -/
def httpCandidateBound : CandidateBound := .unknown

/-- Sources of score for a returned search hit. -/
inductive HitScore where
  | lexical
  | semantic
  | hybridWeightedRanks
  | storeRank
  deriving DecidableEq

/-- The score source on an effective search lane, or the replacement score of a federated merge. -/
def scoreSource : SearchMode → (federated : Bool) → HitScore
  | .lexical, false => .lexical
  | .semantic, false => .semantic
  | .hybrid, false => .hybridWeightedRanks
  | .lexical, true | .semantic, true | .hybrid, true => .storeRank

/-- Accept a comparable band with ordered endpoints both inside the score interval. -/
def admitComparableBand : (floorInRange : Bool) → (ceilingInRange : Bool) → (ordered : Bool) → Bool
  | true, true, true => true
  | true, true, false | true, false, true | true, false, false
  | false, true, true | false, true, false | false, false, true | false, false, false => false

/-- Shape of a lexical declaration at freeze. -/
inductive LanguageDeclaration where
  | pinned
  | emptyUnion
  | nonemptyUnion
  | emittedColumn
  | otherColumn
  deriving DecidableEq

/-- A pinned union needs a first member and a per-row policy needs the emitted column. -/
def admitLanguagePolicy : LanguageDeclaration → Bool
  | .pinned | .nonemptyUnion | .emittedColumn => true
  | .emptyUnion | .otherColumn => false

/-- Unique candidate IDs a spiice passes to the Query authority path in one request. -/
def authorizedCandidateMax : Nat := 2000

/-- The request classes at lexical configuration resolution. -/
inductive LexicalRequest where
  | omitted
  | reliableDetection
  | unreliableDetection
  | recognizedLanguage
  | plausibleConfig
  | malformed
  deriving DecidableEq

/-- What the lexical resolver hands to admission. -/
inductive LexicalResolution where
  | deployment
  | detectedConfig
  | languageConfig
  | explicitConfig
  | refusal
  deriving DecidableEq

/-- Resolve detection and language codes, defer a plausible explicit name to the catalog and refuse malformed input. -/
def lexicalResolution : LexicalRequest → LexicalResolution
  | .omitted | .unreliableDetection => .deployment
  | .reliableDetection => .detectedConfig
  | .recognizedLanguage => .languageConfig
  | .plausibleConfig => .explicitConfig
  | .malformed => .refusal

/-- The retirement routine may remove a non-default configuration without references. -/
def retireConfiguration : (isDefault : Bool) → (referenced : Bool) → Bool
  | false, false => true
  | true, false | true, true | false, true => false

end Proxiima.Search
