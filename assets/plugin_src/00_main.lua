--!strict
--!native
--[[
	VertigoSyncPlugin (Studio plugin)

	Realtime-first sync client for vertigo-sync.

	Design goals:
	- WebSocket streaming by default (`/ws`) with auto-fallback to adaptive polling (`/diff`).
	- High-rate update handling through path-level coalescing and bounded async source fetch workers.
	- Frame-safe apply loop with strict per-heartbeat budget to avoid Studio hitching.
	- Deterministic reconciliation against snapshot/source hashes.
	- Extended file type support (JSON, TXT, CSV, .meta.json properties).
	- Binary model instance creation (feature-gated).
	- DockWidget UI with status, time-travel, and settings panels.
	- Edit-mode builder persistence (feature-gated).

	Server contract (services/vertigo-sync):
	  GET /health
	  GET /snapshot (supports exact history via ?at=<fingerprint>)
	  GET /diff?since=<fingerprint>
	  GET /source/{path}
	  GET /sources
	  GET /sources/content?paths=<csv>
	  GET /events
	  GET /ws
	  GET /history?limit=N
	  GET /model/<path>
	  GET /config
]]

local HttpService = game:GetService("HttpService")
local RunService = game:GetService("RunService")
local TweenService = game:GetService("TweenService")
local Workspace = game:GetService("Workspace")
local ServerScriptService = game:GetService("ServerScriptService")
local ReplicatedStorage = game:GetService("ReplicatedStorage")
local StarterPlayer = game:GetService("StarterPlayer")
local CollectionService = game:GetService("CollectionService")

if not RunService:IsStudio() then
	return
end

-- ─── Constants ────────────────────────────────────────────────────────────────

local CORE = table.freeze({
	LOG_PREFIX = "[VertigoSync]",
	PLUGIN_VERSION = "2026-03-16-v9-trillion-dollar",
	DEFAULT_SERVER_BASE_URL = "http://127.0.0.1:7575",
	DEFAULT_TOOLBAR_ICON_ASSET = "",
	TOOLBAR_ICON_ASSET_SETTING = "VertigoSyncToolbarIconAssetId",
	HEALTH_POLL_SECONDS = 15,
	POLL_INTERVAL_FAST = 0.10,
	POLL_INTERVAL_MAX = 1.50,
	PROJECT_BOOTSTRAP_RETRY_SECONDS = 5.0,
	APPLY_FRAME_BUDGET_SECONDS = 0.002,
	MAX_APPLIES_PER_TICK = 16,
	MAX_FETCH_CONCURRENCY = 8,
	MAX_SOURCE_FETCH_RETRIES = 3,
	MAX_SOURCE_BATCH_SIZE = 8,
	MAX_LUA_SOURCE_LENGTH = 199999,
	MAX_SOURCE_BATCH_ENDPOINT_CHARS = 3500,
	WS_RECONNECT_MIN_SECONDS = 0.25,
	WS_RECONNECT_MAX_SECONDS = 5.0,
	METRIC_FLUSH_SECONDS = 2.0,
	SELF_MUTATION_GUARD_SECONDS = 1.75,
	MANAGED_PATH_ATTR = "VertigoSyncPath",
	MANAGED_SHA_ATTR = "VertigoSyncSha256",
	EDIT_PREVIEW_IGNORE_ATTR = "VertigoSyncEditPreviewIgnore",
})
-- PLUGIN_SEMVER "0.1.0" used inline in status line 3 (no new local to stay under 194 register limit)
local APPLY = table.freeze({
	FRAME_BUDGET_SECONDS = CORE.APPLY_FRAME_BUDGET_SECONDS,
	FRAME_BUDGET_MIN_SECONDS = CORE.APPLY_FRAME_BUDGET_SECONDS * 0.75,
	FRAME_BUDGET_MAX_SECONDS = CORE.APPLY_FRAME_BUDGET_SECONDS * 3.0,
	MAX_PER_TICK = CORE.MAX_APPLIES_PER_TICK,
	MIN_PER_TICK = math.max(4, math.floor(CORE.MAX_APPLIES_PER_TICK * 0.5)),
	MAX_HARD_LIMIT = CORE.MAX_APPLIES_PER_TICK * 4,
	QUEUE_HIGH_WATERMARK = 2048,
	QUEUE_HARD_CAP = 8192,
	BUDGET_EWMA_ALPHA = 0.22,
	BUDGET_RECALC_SECONDS = 0.25,
})

local FETCH = table.freeze({
	CONCURRENCY_MIN = 1,
	CONCURRENCY_MAX = CORE.MAX_FETCH_CONCURRENCY * 3,
})

-- ═══ ARCHITECTURE NOTE ════════════════════════════════════════════════════════
-- Vertigo Sync manages SCRIPT SOURCE ONLY. It syncs .luau/.lua files from
-- disk into the DataModel as Script/ModuleScript instances.
--
-- World geometry (Parts, Models in Workspace) is NEVER touched by sync.
-- The .rbxl file is the source of truth for baked world geometry.
-- Builders are disabled by default to prevent duplicate geometry.
--
-- To regenerate world geometry: enable Builders toggle in the plugin panel,
-- or run the game in Play mode (builders execute via ZoneService:Start).
-- ═══════════════════════════════════════════════════════════════════════════════

-- ─── Feature Gate Constants ─────────────────────────────────────────────────

local FEATURES = table.freeze({
	BINARY_MODELS_ENABLED = false,
	BUILDERS_ENABLED_DEFAULT = false, -- Disabled by default for safety; opt in from the plugin UI or plugin settings.
	TIME_TRAVEL_HISTORY_LIMIT = 256,
	UI_STATUS_REFRESH_SECONDS = 0.5,
	UI_STATUS_REFRESH_HIDDEN_SECONDS = 2.0,
	HISTORY_REFRESH_INTERVAL_SECONDS = 5,
})

-- ─── Instance Pool Constants ─────────────────────────────────────────────────

local POOL = table.freeze({
	SIZE = 128,
	CLASSES = table.freeze({
		"Part",
	"MeshPart",
	"WedgePart",
	"UnionOperation",
	"Model",
	"Folder",
	"Attachment",
	"Weld",
	"Motor6D",
	"Script",
	"LocalScript",
	"ModuleScript",
	"StringValue",
	"LocalizationTable",
	}),
})

local SCRIPT_CLASSES = table.freeze({
	Script = true,
	LocalScript = true,
	ModuleScript = true,
})

-- ─── Builder Constants ───────────────────────────────────────────────────────

local BUILD = table.freeze({
	DEBOUNCE_SECONDS = 0.25,
	FRAME_BUDGET_SECONDS = 1 / 240,
	MAX_BUILDERS_PER_SLICE = 1,
	SLOW_BUILDER_WARN_MS = 8,
})

-- ─── Types ───────────────────────────────────────────────────────────────────

type SyncStatus = "connected" | "disconnected" | "error"
type TransportMode = "idle" | "ws" | "poll"
type ConnectionState = "waiting" | "connecting" | "connected" | "reconnecting" | "error"
type PendingAction = "write" | "delete" | "model_apply"
type ProjectBootstrapMode = "bootstrapping" | "dynamic" | "mismatch"

type PathMapping = {
	prefix: string,
	root: Instance,
	instancePath: string,
	containerSegments: { string },
	boundaryName: string?,
}

type ProjectMappingEntry = {
	fs_path: string,
	instance_path: string,
	class_name: string?,
	ignore_unknown: boolean?,
	properties: { [string]: any }?,
	attributes: { [string]: any }?,
}

type ProjectResponse = {
	name: string,
	project_id: string,
	mappings: { ProjectMappingEntry },
	emit_legacy_scripts: boolean?,
	vertigoSync: ProjectVertigoSyncConfig?,
}

type ProjectBuilderConfig = {
	roots: { string }?,
	dependencyRoots: { string }?,
}

type ProjectEditPreviewConfig = {
	enabled: boolean?,
	builderModulePath: string?,
	builderMethod: string?,
	watchRoots: { string }?,
	debounceSeconds: number?,
	rootRefreshSeconds: number?,
	mode: string?,
}

type ProjectVertigoSyncConfig = {
	builders: ProjectBuilderConfig?,
	editPreview: ProjectEditPreviewConfig?,
}

type PendingOp = {
	action: PendingAction,
	epoch: number,
	retries: number,
	queued: boolean,
	expectedSha: string?,
}

type ReadySource = {
	epoch: number,
	source: string,
	sha256: string?,
	meta: EntryMeta?,
}

type FetchTask = {
	path: string,
	epoch: number,
}

type EntryMeta = {
	properties: { [string]: any }?,
	attributes: { [string]: any }?,
}

type SnapshotEntry = {
	path: string,
	sha256: string,
	bytes: number,
	file_type: string?,
	meta: EntryMeta?,
}

type DiffAddedEntry = {
	path: string,
	sha256: string,
	bytes: number,
	file_type: string?,
	meta: EntryMeta?,
}

type DiffModifiedEntry = {
	path: string,
	current_sha256: string,
	current_bytes: number,
	previous_sha256: string?,
	previous_bytes: number?,
	file_type: string?,
	meta: EntryMeta?,
}

type DiffDeletedEntry = {
	path: string,
	sha256: string,
	bytes: number,
}

type SnapshotResponse = {
	fingerprint: string,
	entries: { SnapshotEntry },
}

type DiffRenamedEntry = {
	old_path: string,
	new_path: string,
	sha256: string,
	bytes: number,
}

type DiffResponse = {
	previous_fingerprint: string,
	current_fingerprint: string,
	added: { DiffAddedEntry },
	modified: { DiffModifiedEntry },
	deleted: { DiffDeletedEntry },
	renamed: { DiffRenamedEntry }?,
}

type SourceContentEntry = {
	path: string,
	sha256: string?,
	bytes: number,
	content: string,
	meta: EntryMeta?,
}

type SourcesContentResponse = {
	entries: { SourceContentEntry },
	missing: { string }?,
}

type HistoryEntry = {
	seq: number,
	fingerprint: string,
	timestamp: string,
	added: number,
	modified: number,
	deleted: number,
	geometry_affecting: boolean?,
	scope: string?,
}

type ModelInstance = {
	index: number,
	parentIndex: number?,
	name: string,
	className: string,
	properties: { [string]: any }?,
}

type ModelManifest = {
	instances: { ModelInstance },
	rootCount: number,
}

-- A single pending model instance creation op, queued for frame-budgeted apply.
type ModelApplyOp = {
	manifestPath: string,
	manifestEpoch: number,
	instanceIndex: number,
	entry: ModelInstance,
}

-- Ready model manifest waiting for staged apply.
type ReadyModel = {
	epoch: number,
	manifest: ModelManifest,
	sha256: string?,
}

-- ─── Services / Optional Capability Detection ───────────────────────────────

local WebSocketService: any = nil
local wsServiceOk, wsServiceOrError = pcall(function()
	return game:GetService("WebSocketService")
end)
if wsServiceOk then
	WebSocketService = wsServiceOrError
end

local starterPlayerScripts = StarterPlayer:FindFirstChild("StarterPlayerScripts")
if starterPlayerScripts == nil then
	starterPlayerScripts = StarterPlayer:WaitForChild("StarterPlayerScripts")
end

-- ─── Frozen Lookup Tables ────────────────────────────────────────────────────

local CLASS_MAP: { [string]: string } = table.freeze({
	["init.server.luau"] = "Script",
	["init.client.luau"] = "LocalScript",
	["init.luau"] = "ModuleScript",
	["init.server.lua"] = "Script",
	["init.client.lua"] = "LocalScript",
	["init.lua"] = "ModuleScript",
})

local INIT_FILES: { [string]: boolean } = table.freeze({
	["init.server.luau"] = true,
	["init.client.luau"] = true,
	["init.luau"] = true,
	["init.server.lua"] = true,
	["init.client.lua"] = true,
	["init.lua"] = true,
})

-- ─── State ───────────────────────────────────────────────────────────────────

local syncEnabled = true
local currentStatus: SyncStatus = "disconnected"
local transportMode: TransportMode = "idle"

local lastHash: string? = nil
local lastHealthCheckAt = 0.0
local nextPollAt = 0.0
local nextProjectBootstrapAttemptAt = 0.0
local pollInterval = CORE.POLL_INTERVAL_FAST

local consecutiveErrors = 0
local reconnectCount = 0
local laggedEvents = 0
local droppedUpdates = 0

local selfMutationGuardUntil = 0.0
local resyncRequested = true

-- ─── Connection State Machine ───────────────────────────────────────────────

local connectionState: ConnectionState = "waiting"
local hasEverConnected = false
local connectionReconnectAttempt = 0

local wsSocket: any = nil
local wsConnected = false
local wsReconnectBackoffSeconds = CORE.WS_RECONNECT_MIN_SECONDS
local nextWsConnectAt = 0.0

local opEpoch = 0
local pendingOps: { [string]: PendingOp } = {}
local pendingQueue: { string } = {}
local pendingQueueHead = 1

local fetchQueue: { FetchTask } = {}
local fetchQueueHead = 1
local fetchQueuedEpoch: { [string]: number } = {}
local inflightFetchEpoch: { [string]: number } = {}
local fetchInFlight = 0

local readySources: { [string]: ReadySource } = {}

local managedIndex: { [string]: Instance } = {}
local managedShaByPath: { [string]: string } = {}
local hardRejectedShaByPath: { [string]: string } = {}
local hardRejectedReasonByPath: { [string]: string } = {}

local applyWindowStart = os.clock()
local appliedInWindow = 0
local appliedPerSecond = 0
local lastMetricFlushAt = 0.0
local adaptiveApplyBudgetSeconds = APPLY.FRAME_BUDGET_SECONDS
local adaptiveMaxAppliesPerTick = APPLY.MAX_PER_TICK
local adaptiveFetchConcurrency = CORE.MAX_FETCH_CONCURRENCY
local applyCostEwmaSeconds = 0.0
local lastAdaptiveRecalcAt = 0.0

-- Metadata cache: path -> EntryMeta from snapshot/diff entries
local metaByPath: { [string]: EntryMeta } = {}

-- ─── Binary Model Apply State ────────────────────────────────────────────────

-- Ready model manifests waiting to be staged into the apply queue
local readyModels: { [string]: ReadyModel } = {}

-- Per-path queue of model instance creation ops (topologically ordered)
local modelApplyQueues: { [string]: { ModelApplyOp } } = {}
local modelApplyQueueHeads: { [string]: number } = {}

-- Per-path lookup: instanceIndex -> Instance (built during staged apply)
local modelBuildLookup: { [string]: { [number]: Instance } } = {}

-- Paths with active model applies in progress
local modelApplyActive: { [string]: boolean } = {}

-- ─── Instance Pool State ─────────────────────────────────────────────────────

local instancePool: { [string]: { Instance } } = {}
local SETTINGS

-- ─── Time-Travel State ──────────────────────────────────────────────────────

local HISTORY = {
	entries = {} :: { HistoryEntry },
	currentIndex = 0, -- 0 = live mode
	loaded = false,
	fetchFailed = false,
	fetchInFlight = false,
	lastFetchAt = 0,
	busy = false,
	active = false,
	pendingIndex = nil :: number?,
	pendingResumeLive = false,
	needsBuilderReconcile = false,
}

local function bumpTimeTravelEpoch(): number
	local currentEpoch: any = Workspace:GetAttribute("VertigoSyncTimeTravelEpoch")
	local nextEpoch: number = if type(currentEpoch) == "number" then (currentEpoch + 1) else 1
	Workspace:SetAttribute("VertigoSyncTimeTravelEpoch", nextEpoch)
	return nextEpoch
end

local function bumpPreviewInvalidationEpoch(): number
	local currentEpoch: any = Workspace:GetAttribute("VertigoSyncPreviewInvalidationEpoch")
	local nextEpoch: number = if type(currentEpoch) == "number" then (currentEpoch + 1) else 1
	Workspace:SetAttribute("VertigoSyncPreviewInvalidationEpoch", nextEpoch)
	return nextEpoch
end

local function historyTransitionAffectsPreview(fromIndex: number, toIndex: number): boolean
	if fromIndex == toIndex then
		return false
	end

	local lowerExclusive: number
	local upperInclusive: number
	if fromIndex == 0 then
		lowerExclusive = toIndex
		upperInclusive = #HISTORY.entries
	elseif toIndex == 0 then
		lowerExclusive = fromIndex
		upperInclusive = #HISTORY.entries
	else
		lowerExclusive = math.min(fromIndex, toIndex)
		upperInclusive = math.max(fromIndex, toIndex)
	end

	for index = lowerExclusive + 1, upperInclusive do
		local entry = HISTORY.entries[index]
		if entry and entry.geometry_affecting then
			return true
		end
	end

	return false
end

local function isTimeTravelHardPauseActive(): boolean
	return SETTINGS.timeTravelUI and HISTORY.active and HISTORY.currentIndex > 0
end

local function publishTimeTravelAttributes()
	Workspace:SetAttribute("VertigoSyncTimeTravel", HISTORY.active)
	if HISTORY.active and HISTORY.currentIndex > 0 and HISTORY.entries[HISTORY.currentIndex] ~= nil then
		Workspace:SetAttribute("VertigoSyncTimeTravelSeq", HISTORY.entries[HISTORY.currentIndex].seq)
		Workspace:SetAttribute("VertigoSyncTimeTravelFingerprint", HISTORY.entries[HISTORY.currentIndex].fingerprint)
	else
		Workspace:SetAttribute("VertigoSyncTimeTravelSeq", nil)
		Workspace:SetAttribute("VertigoSyncTimeTravelFingerprint", nil)
	end
	Workspace:SetAttribute("VertigoSyncTimeTravelHardPause", isTimeTravelHardPauseActive())
	local state = "live"
	if HISTORY.busy and HISTORY.active and HISTORY.currentIndex > 0 then
		state = "rewinding"
	elseif HISTORY.busy then
		state = "resuming"
	elseif HISTORY.active and HISTORY.currentIndex > 0 then
		state = "frozen"
	end
	Workspace:SetAttribute("VertigoSyncTimeTravelState", state)
end

-- ─── Builder State ───────────────────────────────────────────────────────────

local BUILDERS = {
	enabled = false, -- set in init based on edit mode
	sources = {} :: { [string]: string }, -- path -> source hash
	outputTags = {} :: { [string]: string }, -- path -> output tag
	dependencyMap = {} :: { [string]: { [string]: boolean } }, -- shared module path -> set of builder paths
	dirtySet = {} :: { [string]: boolean }, -- builder paths pending re-execution
	forceSet = {} :: { [string]: boolean }, -- builder paths that must re-run even if source hash is unchanged
	queue = {} :: { string },
	queueHead = 1,
	queuedSet = {} :: { [string]: boolean },
	debounceScheduled = false,
	pumpScheduled = false,
	pumpActive = false,
	initialPending = 0,
	initialExecuted = 0,
	initialSkipped = 0,
	initialStartedAt = 0,
	lastInitialQueuedFingerprint = "",
	lastInitialCompletedFingerprint = "",
}

local PERF = {
	builderLastMs = 0.0,
	builderAvgMs = 0.0,
	builderMaxMs = 0.0,
	builderSamples = 0,
	builderLastPath = "",
	builderLastResult = "",
}

-- ─── Settings State ──────────────────────────────────────────────────────────

SETTINGS = {
	binaryModels = FEATURES.BINARY_MODELS_ENABLED,
	buildersEnabled = FEATURES.BUILDERS_ENABLED_DEFAULT,
	timeTravelUI = true,
	historyBuffer = FEATURES.TIME_TRAVEL_HISTORY_LIMIT,
	applyQueueLimit = 0, -- 0 = unlimited
}

local TIME_TRAVEL_LIST_BOTTOM_PADDING = 4

-- Log level: 0=quiet, 1=normal, 2=verbose (controllable via sync_plugin_command)
local logLevel: number = 1

-- ─── Plugin Boot Tracking ────────────────────────────────────────────────────

local pluginBootTime: number = os.clock()
local serverBootTimeCache: number? = nil -- cached server_boot_time from /health
local serverIdCache: string? = nil -- cached server_id from /health

-- ─── Project Bootstrap State ────────────────────────────────────────────────

local PROJECT = {
	mappings = {} :: { PathMapping },
	prefixLens = {} :: { number },
	mode = "bootstrapping" :: ProjectBootstrapMode,
	message = "Waiting for /project",
	name = nil :: string?,
	projectId = nil :: string?,
	mappingCount = 0,
	loaded = false,
	blocked = false,
	emitLegacyScripts = true,
	builderRoots = {} :: { string },
	builderDependencyRoots = {} :: { string },
	lastStatusToastKey = "",
	lastReadinessKey = "",
	attachedRootGuards = {} :: { [Instance]: boolean },
	editPreview = {
		enabled = false,
		builderModulePath = "",
		builderMethod = "Build",
		watchRoots = {} :: { string },
		debounceSeconds = BUILD.DEBOUNCE_SECONDS,
		rootRefreshSeconds = 1.0,
		mode = "edit_only",
		rootConnections = {} :: { [string]: { root: Instance, connections: { RBXScriptConnection } } },
		sourceConnections = {} :: { [Instance]: RBXScriptConnection },
		pendingReason = nil :: string?,
		buildScheduled = false,
		buildInProgress = false,
		consecutiveFailures = 0,
		lastSkipAt = 0.0,
		lastSkipSignature = nil :: string?,
		nextRootRefreshAt = 0.0,
		initialBuildQueued = false,
		scheduleEpoch = 0,
	},
}
local SERVER = {
	activeBaseUrl = nil :: string?,
	lastGoodBaseUrl = nil :: string?,
	lastGoodProjectId = nil :: string?,
	allowUntrustedDiscovery = false,
	discoveryError = "",
	liveSyncSkipAggregation = nil :: {
		context: string,
		count: number,
		samplePaths: { string },
	}?,
}
local Runtime = {}
local resolveMapping: (filePath: string) -> (PathMapping?, string?)
local bootstrapManagedIndex: () -> ()
local closeWebSocket: (reason: string) -> ()
local runInitialBuilders: () -> ()

-- ─── Forward Declarations (UI functions used by sync logic) ──────────────────

-- Toast colors (forward-declared; actual Color3 values assigned after theme setup)
local TOAST_COLOR_SUCCESS: Color3 = Color3.fromRGB(52, 199, 89)
local TOAST_COLOR_ERROR: Color3 = Color3.fromRGB(255, 69, 58)
local TOAST_COLOR_INFO: Color3 = Color3.fromRGB(56, 132, 244)

-- Forward-declared toast function; assigned after UI init
local showToast: (message: string, toastColor: Color3?) -> () = function(_message: string, _toastColor: Color3?)
	-- No-op until UI is initialized
end

-- ─── State Reporting Constants ──────────────────────────────────────────────

local STATE_REPORT_INTERVAL_SECONDS = 3
local MANAGED_REPORT_INTERVAL_SECONDS = 30
local LOG_THROTTLE_SECONDS = 10

-- ─── Logging ─────────────────────────────────────────────────────────────────

local function info(message: string)
	if logLevel < 1 then
		return
	end
	print(string.format("%s %s", CORE.LOG_PREFIX, message))
end

local function warnMsg(message: string)
	warn(string.format("%s %s", CORE.LOG_PREFIX, message))
end

-- Throttled logging: suppress identical messages within LOG_THROTTLE_SECONDS
local lastLogByKey: { [string]: number } = {}

local function throttledLog(key: string, message: string, isWarn: boolean)
	local now: number = os.clock()
	local last: number = lastLogByKey[key] or 0
	if now - last < LOG_THROTTLE_SECONDS then
		return
	end
	lastLogByKey[key] = now
	if isWarn then
		warn(CORE.LOG_PREFIX .. " " .. message)
	else
		print(CORE.LOG_PREFIX .. " " .. message)
	end
end

@native
local function clampNumber(value: number, minValue: number, maxValue: number): number
	if value < minValue then
		return minValue
	end
	if value > maxValue then
		return maxValue
	end
	return value
end

-- ─── Guard / Mode Helpers ────────────────────────────────────────────────────

@native
local function refreshSelfMutationGuard()
	local guardUntil: number = os.clock() + CORE.SELF_MUTATION_GUARD_SECONDS
	if guardUntil > selfMutationGuardUntil then
		selfMutationGuardUntil = guardUntil
	end
end

@native
local function inSelfMutationGuard(): boolean
	return os.clock() < selfMutationGuardUntil
end

@native
function Runtime.isManagedMutationInstance(instance: Instance): boolean
	local managedPath: any = instance:GetAttribute(CORE.MANAGED_PATH_ATTR)
	if type(managedPath) == "string" and managedPath ~= "" then
		return true
	end
	local managedSha: any = instance:GetAttribute(CORE.MANAGED_SHA_ATTR)
	return type(managedSha) == "string" and managedSha ~= ""
end

function Runtime.isEditPreviewIgnoredInstance(instance: Instance): boolean
	local node: Instance? = instance
	while node ~= nil do
		local ignored: any = node:GetAttribute(CORE.EDIT_PREVIEW_IGNORE_ATTR)
		if ignored == true then
			return true
		end
		node = node.Parent
	end
	return false
end

function Runtime.isEditPreviewGeometryAffectingInstance(instance: Instance): boolean
	if instance:IsA("LocalizationTable") then
		return false
	end

	local fullName = instance:GetFullName()
	if fullName == "Lighting" or string.find(fullName, "Lighting.", 1, true) == 1 then
		return false
	end
	if string.find(fullName, "ServerScriptService.ImportService.DayNightCycle", 1, true) then
		return false
	end
	if string.find(fullName, "ServerScriptService.ImportService.SceneAudit", 1, true) then
		return false
	end

	return true
end

@native
local function isEditMode(): boolean
	return RunService:IsEdit() and not RunService:IsRunning()
end

@native
local function isStudioSyncMode(): boolean
	return RunService:IsStudio() and (isEditMode() or (RunService:IsRunning() and RunService:IsServer()))
end

local function describeStudioMode(): string
	local parts = table.create(4)
	if RunService:IsEdit() then
		table.insert(parts, "edit")
	end
	if RunService:IsRunning() then
		table.insert(parts, "running")
	end
	if RunService:IsServer() then
		table.insert(parts, "server")
	elseif RunService:IsClient() then
		table.insert(parts, "client")
	end
	if #parts == 0 then
		table.insert(parts, "unknown")
	end
	return table.concat(parts, "+")
end

-- ─── Attributes / Telemetry ─────────────────────────────────────────────────

function Runtime.evaluateProjectReadiness(): (boolean, string, string)
	if not syncEnabled then
		return false, "sync_disabled", "Sync is disabled in the plugin toolbar."
	end
	if not isStudioSyncMode() then
		return false, "studio_mode_unsupported", string.format("Sync is unavailable in Studio mode %s.", describeStudioMode())
	end
	if PROJECT.blocked then
		if PROJECT.message ~= "" then
			return false, "project_blocked", PROJECT.message
		end
		return false, "project_blocked", "Project bootstrap is blocked."
	end
	if not PROJECT.loaded then
		if PROJECT.message ~= "" then
			return false, "project_bootstrap_pending", PROJECT.message
		end
		return false, "project_bootstrap_pending", "Waiting for /project"
	end
	if PROJECT.editPreview.enabled and PROJECT.editPreview.builderModulePath == "" then
		return false, "edit_preview_misconfigured", "Edit preview is enabled, but builderModulePath is missing."
	end
	if currentStatus == "error" then
		if PROJECT.message ~= "" then
			return false, "sync_error", PROJECT.message
		end
		return false, "sync_error", "Sync is in an error state."
	end
	if currentStatus ~= "connected" then
		if hasEverConnected then
			return false, "sync_disconnected", "Sync is disconnected from the server."
		end
		return false, "sync_disconnected", "Waiting for the initial sync connection."
	end
	return true, "ready", "Project is ready for sync and edit preview."
end

@native
function Runtime.isEditPreviewSuspended(): (boolean, string?)
	local suspended: any = Workspace:GetAttribute("VertigoSyncEditPreviewSuspended")
	if suspended == true then
		local reason: any = Workspace:GetAttribute("VertigoSyncEditPreviewSuspendReason")
		if type(reason) == "string" and reason ~= "" then
			return true, reason
		end
		return true, "suspended"
	end
	return false, nil
end

@native
function Runtime.isRunAllSuiteActive(): boolean
	return Workspace:GetAttribute("VertigoSyncRunAllActive") == true
end

local pluginCommandBusy: boolean = false

function Runtime.updatePluginFactAttributes()
	Workspace:SetAttribute("VertigoSyncProjectReadinessReady", nil)
	Workspace:SetAttribute("VertigoSyncProjectReadinessCode", nil)
	Workspace:SetAttribute("VertigoSyncProjectReadinessMessage", nil)
	Workspace:SetAttribute("VertigoSyncPluginConnectionStatus", currentStatus)
	Workspace:SetAttribute("VertigoSyncPluginTransportMode", transportMode)
	Workspace:SetAttribute("VertigoSyncPluginConnected", currentStatus == "connected")
	Workspace:SetAttribute("VertigoSyncPluginProjectLoaded", PROJECT.loaded)
	Workspace:SetAttribute("VertigoSyncPluginSnapshotHash", lastHash)
	Workspace:SetAttribute("VertigoSyncPluginSnapshotApplyInProgress", HISTORY.busy or fetchInFlight > 0)
	Workspace:SetAttribute("VertigoSyncPluginCommandBusy", pluginCommandBusy)

	local snapshotState: string
	if HISTORY.fetchFailed then
		snapshotState = "fetch_failed"
	elseif HISTORY.busy then
		snapshotState = "apply_in_progress"
	elseif HISTORY.active then
		snapshotState = "historical"
	elseif resyncRequested then
		snapshotState = "resync_requested"
	elseif fetchInFlight > 0 then
		snapshotState = "fetching"
	elseif PROJECT.loaded then
		snapshotState = "live"
	else
		snapshotState = "project_pending"
	end
	Workspace:SetAttribute("VertigoSyncPluginSnapshotState", snapshotState)
end

local function setStatusAttributes(status: SyncStatus, hash: string?)
	currentStatus = status
	Workspace:SetAttribute("VertigoSyncStatus", status)
	if hash then
		Workspace:SetAttribute("VertigoSyncHash", hash)
	end
	Workspace:SetAttribute("VertigoSyncLastUpdate", os.date("!%Y-%m-%dT%H:%M:%SZ"))
	Runtime.updatePluginFactAttributes()
end

local function setProjectStatus(mode: ProjectBootstrapMode, message: string, projectName: string?, blocked: boolean)
	PROJECT.mode = mode
	PROJECT.message = message
	PROJECT.name = projectName
	PROJECT.blocked = blocked

	Workspace:SetAttribute("VertigoSyncProjectMode", mode)
	Workspace:SetAttribute("VertigoSyncProjectName", projectName)
	Workspace:SetAttribute("VertigoSyncProjectId", PROJECT.projectId)
	Workspace:SetAttribute("VertigoSyncProjectMessage", message)
	Workspace:SetAttribute("VertigoSyncProjectBlocked", blocked)
	Workspace:SetAttribute("VertigoSyncProjectMismatch", mode == "mismatch")
	Workspace:SetAttribute("VertigoSyncProjectMappingCount", PROJECT.mappingCount)
	Workspace:SetAttribute("VertigoSyncEmitLegacyScripts", PROJECT.emitLegacyScripts)
	Runtime.updatePluginFactAttributes()

	if mode == "mismatch" and message ~= "" then
		local toastKey = "mismatch::" .. message
		if toastKey ~= PROJECT.lastStatusToastKey then
			PROJECT.lastStatusToastKey = toastKey
			showToast(message, TOAST_COLOR_ERROR)
		end
	end
end

local function updateBuilderPerfAttributes()
	local queueDepth: number = 0
	if BUILDERS.queueHead <= #BUILDERS.queue then
		queueDepth = #BUILDERS.queue - BUILDERS.queueHead + 1
	end
	Workspace:SetAttribute("VertigoSyncBuilderQueueDepth", queueDepth)
	Workspace:SetAttribute("VertigoSyncBuilderPumpActive", BUILDERS.pumpActive)
	Workspace:SetAttribute("VertigoSyncBuilderLastMs", PERF.builderLastMs)
	Workspace:SetAttribute("VertigoSyncBuilderAvgMs", PERF.builderAvgMs)
	Workspace:SetAttribute("VertigoSyncBuilderMaxMs", PERF.builderMaxMs)
	Workspace:SetAttribute("VertigoSyncBuilderLastPath", PERF.builderLastPath)
	Workspace:SetAttribute("VertigoSyncBuilderLastResult", PERF.builderLastResult)
end

local function recordBuilderPerf(path: string, result: string, elapsedMs: number)
	PERF.builderLastMs = elapsedMs
	PERF.builderLastPath = path
	PERF.builderLastResult = result
	PERF.builderSamples += 1
	if PERF.builderSamples == 1 then
		PERF.builderAvgMs = elapsedMs
	else
		PERF.builderAvgMs = PERF.builderAvgMs * 0.85 + elapsedMs * 0.15
	end
	if elapsedMs > PERF.builderMaxMs then
		PERF.builderMaxMs = elapsedMs
	end
	updateBuilderPerfAttributes()
	if elapsedMs >= BUILD.SLOW_BUILDER_WARN_MS then
		throttledLog(
			"slow_builder_" .. path,
			string.format("Builder slice over budget: %s %.1fms (%s)", path, elapsedMs, result),
			false
		)
	end
end

function PERF.compactMetricNumber(value: number): string
	local absValue = math.abs(value)
	if absValue >= 1000000 then
		return string.format("%.1fm", value / 1000000)
	end
	if absValue >= 1000 then
		return string.format("%.1fk", value / 1000)
	end
	return tostring(math.floor(value + 0.5))
end

function PERF.builderStatusSummary(): string
	if not SETTINGS.buildersEnabled then
		return "build off"
	end

	local rootCount: number = #PROJECT.builderRoots
	if rootCount == 0 then
		return "build none"
	end

	local queueDepth: any = Workspace:GetAttribute("VertigoSyncBuilderQueueDepth")
	local avgMs: any = Workspace:GetAttribute("VertigoSyncBuilderAvgMs")
	local pumpActive: any = Workspace:GetAttribute("VertigoSyncBuilderPumpActive")
	local lastResult: any = Workspace:GetAttribute("VertigoSyncBuilderLastResult")

	local queuedCount: number = if type(queueDepth) == "number" then math.max(queueDepth, BUILDERS.initialPending) else BUILDERS.initialPending
	local avg: number = if type(avgMs) == "number" then avgMs else 0
	local active: boolean = pumpActive == true
	local resultText: string = if lastResult == "executed"
		then "cur"
		elseif lastResult == "skipped"
		then "ok"
		elseif lastResult == "failed"
		then "fail"
		else "ready"

	if active then
		return string.format("build q%s %.0f", PERF.compactMetricNumber(queuedCount), avg)
	end
	if queuedCount > 0 then
		return string.format("build q%s", PERF.compactMetricNumber(queuedCount))
	end
	if resultText ~= "ready" or avg > 0 then
		return string.format("build %s %.0f", resultText, avg)
	end
	return string.format("build ok %d", rootCount)
end

function PERF.previewStatusSummary(): string
	local state: any = Workspace:GetAttribute("VertigoPreviewSyncState")
	local phase: any = Workspace:GetAttribute("VertigoPreviewSyncPhase")
	local loaded: any = Workspace:GetAttribute("VertigoPreviewSyncLoadedChunks")
	local target: any = Workspace:GetAttribute("VertigoPreviewSyncTargetChunks")
	local avgMs: any = Workspace:GetAttribute("VertigoPreviewAvgMs")

	local stateText: string = if type(state) == "string" and state ~= "" then state else "idle"
	local phaseText: string = if type(phase) == "string" and phase ~= "" then phase else "idle"
	local loadedCount: number = if type(loaded) == "number" then loaded else 0
	local targetCount: number = if type(target) == "number" then target else 0
	local avg: number = if type(avgMs) == "number" then avgMs else 0

	if stateText == "running" or stateText == "scheduled" then
		return string.format("prev %s %s/%s", string.sub(phaseText, 1, 2), PERF.compactMetricNumber(loadedCount), PERF.compactMetricNumber(targetCount))
	end
	if stateText == "failed" or stateText == "error" then
		return "prev fail"
	end
	if targetCount > 0 or avg > 0 then
		return string.format("prev %s %s/%s", string.sub(stateText, 1, 2), PERF.compactMetricNumber(loadedCount), PERF.compactMetricNumber(targetCount))
	end
	return "prev idle"
end

@native
-- METRIC_DEBUG_VERBOSE: set to true below to emit all diagnostic attributes
local function flushMetrics(force: boolean)
	local now = os.clock()
	if not force and now - lastMetricFlushAt < CORE.METRIC_FLUSH_SECONDS then
		return
	end
	lastMetricFlushAt = now

	-- Essential metrics (always emitted)
	Workspace:SetAttribute("VertigoSyncQueueDepth", math.max(#pendingQueue - pendingQueueHead + 1, 0))
	Workspace:SetAttribute("VertigoSyncPluginVersion", CORE.PLUGIN_VERSION)

	-- Verbose diagnostics (gated behind debug flag to reduce SetAttribute overhead)
	if false then -- METRIC_DEBUG_VERBOSE: change to true for diagnostic attributes
		Workspace:SetAttribute("VertigoSyncTransport", transportMode)
		Workspace:SetAttribute("VertigoSyncFetchQueueDepth", math.max(#fetchQueue - fetchQueueHead + 1, 0))
		Workspace:SetAttribute("VertigoSyncFetchInFlight", fetchInFlight)
		Workspace:SetAttribute("VertigoSyncLaggedEvents", laggedEvents)
		Workspace:SetAttribute("VertigoSyncDroppedUpdates", droppedUpdates)
		Workspace:SetAttribute("VertigoSyncReconnects", reconnectCount)
		Workspace:SetAttribute("VertigoSyncAppliedPerSecond", appliedPerSecond)
		Workspace:SetAttribute("VertigoSyncApplyBudgetMs", math.floor(adaptiveApplyBudgetSeconds * 1000 + 0.5))
		Workspace:SetAttribute("VertigoSyncApplyMaxPerTick", adaptiveMaxAppliesPerTick)
		Workspace:SetAttribute("VertigoSyncFetchConcurrency", adaptiveFetchConcurrency)
		Workspace:SetAttribute("VertigoSyncApplyCostUs", math.floor(applyCostEwmaSeconds * 1000000 + 0.5))
		Workspace:SetAttribute("VertigoSyncRealtimeDefault", true)
		Workspace:SetAttribute("VertigoSyncBinaryModels", SETTINGS.binaryModels)
		Workspace:SetAttribute("VertigoSyncBuildersEnabled", BUILDERS.enabled)
	end
	publishTimeTravelAttributes()
end

-- ─── URL / HTTP helpers ─────────────────────────────────────────────────────

@native
local function wsUrlFromHttpBase(httpBase: string): string
	if string.sub(httpBase, 1, 8) == "https://" then
		return "wss://" .. string.sub(httpBase, 9) .. "/ws"
	end
	if string.sub(httpBase, 1, 7) == "http://" then
		return "ws://" .. string.sub(httpBase, 8) .. "/ws"
	end
	return httpBase .. "/ws"
end

@native
local function encodePathForRoute(path: string): string
	local segments: { string } = string.split(path, "/")
	local segmentCount: number = #segments
	local encodedSegments: { string } = table.create(segmentCount)
	for i = 1, segmentCount do
		local seg: string = segments[i]
		if seg ~= "" then
			table.insert(encodedSegments, HttpService:UrlEncode(seg))
		end
	end
	return table.concat(encodedSegments, "/")
end

local function normalizeServerBaseUrl(rawValue: any): string?
	if type(rawValue) ~= "string" or rawValue == "" then
		return nil
	end
	local trimmed = string.gsub(rawValue, "%s+", "")
	trimmed = string.gsub(trimmed, "/+$", "")
	if trimmed == "" then
		return nil
	end
	if string.sub(trimmed, 1, 7) ~= "http://" and string.sub(trimmed, 1, 8) ~= "https://" then
		return nil
	end
	return trimmed
end

local function normalizeProjectId(rawValue: any): string?
	if type(rawValue) ~= "string" or rawValue == "" then
		return nil
	end
	local trimmed = string.gsub(rawValue, "^%s+", "")
	trimmed = string.gsub(trimmed, "%s+$", "")
	if trimmed == "" then
		return nil
	end
	return trimmed
end

local function readConfiguredServerBaseUrl(): string?
	local workspaceUrl = normalizeServerBaseUrl(Workspace:GetAttribute("VertigoSyncServerUrl"))
	if workspaceUrl ~= nil then
		return workspaceUrl
	end
	return normalizeServerBaseUrl(plugin:GetSetting("VertigoSyncServerUrl"))
end

local function readExpectedProjectId(): string?
	local workspaceId = normalizeProjectId(Workspace:GetAttribute("VertigoSyncProjectId"))
	if workspaceId ~= nil then
		return workspaceId
	end
	local configuredId = normalizeProjectId(plugin:GetSetting("VertigoSyncProjectId"))
	if configuredId ~= nil then
		return configuredId
	end
	return normalizeProjectId(plugin:GetSetting("VertigoSyncLastGoodProjectId"))
end

local function rememberProjectBinding(baseUrl: string, projectId: string)
	SERVER.activeBaseUrl = baseUrl
	SERVER.lastGoodBaseUrl = baseUrl
	SERVER.lastGoodProjectId = projectId
	pcall(function()
		plugin:SetSetting("VertigoSyncLastGoodServerUrl", baseUrl)
		plugin:SetSetting("VertigoSyncLastGoodProjectId", projectId)
	end)
end

local function getServerBaseUrl(): string
	local configuredUrl = readConfiguredServerBaseUrl()
	if configuredUrl ~= nil then
		SERVER.activeBaseUrl = configuredUrl
		return configuredUrl
	end
	if SERVER.activeBaseUrl ~= nil then
		return SERVER.activeBaseUrl
	end
	local lastGoodUrl = normalizeServerBaseUrl(plugin:GetSetting("VertigoSyncLastGoodServerUrl"))
	if lastGoodUrl ~= nil then
		SERVER.lastGoodBaseUrl = lastGoodUrl
		SERVER.lastGoodProjectId = normalizeProjectId(plugin:GetSetting("VertigoSyncLastGoodProjectId"))
		SERVER.activeBaseUrl = lastGoodUrl
		return lastGoodUrl
	end
	return CORE.DEFAULT_SERVER_BASE_URL
end

local function requestRaw(endpoint: string, baseUrlOverride: string?): (boolean, any)
	local url = (baseUrlOverride or getServerBaseUrl()) .. endpoint
	local ok, result = pcall(function()
		return HttpService:RequestAsync({
			Url = url,
			Method = "GET",
			Headers = {
				["Accept"] = "application/json, text/plain; q=0.9",
			},
		})
	end)
	if not ok then
		return false, tostring(result)
	end
	return true, result
end

local function requestJson(endpoint: string, baseUrlOverride: string?): (boolean, any, number)
	local ok, rawOrErr = requestRaw(endpoint, baseUrlOverride)
	if not ok then
		return false, rawOrErr, 0
	end
	local raw = rawOrErr
	if raw.StatusCode < 200 or raw.StatusCode >= 300 then
		return false, string.format("HTTP %d: %s", raw.StatusCode, tostring(raw.Body or "")), raw.StatusCode
	end
	local decodeOk, decoded = pcall(function()
		return HttpService:JSONDecode(raw.Body)
	end)
	if not decodeOk then
		return false, string.format("JSON decode failed: %s", tostring(decoded)), raw.StatusCode
	end
	return true, decoded, raw.StatusCode
end

local function requestDiscover(baseUrlOverride: string?): (boolean, any, number)
	local ok, payloadOrErr, statusCode = requestJson("/discover", baseUrlOverride)
	if not ok then
		return false, payloadOrErr, statusCode
	end
	if
		type(payloadOrErr) ~= "table"
		or type(payloadOrErr.project_id) ~= "string"
		or type(payloadOrErr.project_name) ~= "string"
		or type(payloadOrErr.server_id) ~= "string"
	then
		return false, "malformed /discover payload", statusCode
	end
	return true, payloadOrErr, statusCode
end

local function discoverServerBaseUrl(allowUntrustedDiscovery: boolean?): string?
	SERVER.discoveryError = ""
	if readConfiguredServerBaseUrl() ~= nil then
		return nil
	end

	local canAdoptUntrusted = allowUntrustedDiscovery == true or SERVER.allowUntrustedDiscovery
	local expectedProjectId = readExpectedProjectId()
	local candidates: { string } = {}
	local seen: { [string]: boolean } = {}
	local healthyCandidates: { string } = {}
	local matchingCandidates: { string } = {}
	local projectIdByCandidate: { [string]: string } = {}

	local function pushCandidate(rawValue: any)
		local normalized = normalizeServerBaseUrl(rawValue)
		if normalized == nil or seen[normalized] then
			return
		end
		seen[normalized] = true
		table.insert(candidates, normalized)
	end

	pushCandidate(Workspace:GetAttribute("VertigoSyncServerUrl"))
	pushCandidate(plugin:GetSetting("VertigoSyncServerUrl"))
	pushCandidate(plugin:GetSetting("VertigoSyncLastGoodServerUrl"))
	pushCandidate(SERVER.activeBaseUrl)
	pushCandidate(CORE.DEFAULT_SERVER_BASE_URL)

	for i = 1, #candidates do
		local candidate = candidates[i]
		local ok, payloadOrErr, _statusCode = requestDiscover(candidate)
		if ok then
			table.insert(healthyCandidates, candidate)
			projectIdByCandidate[candidate] = payloadOrErr.project_id
			if expectedProjectId == nil or payloadOrErr.project_id == expectedProjectId then
				table.insert(matchingCandidates, candidate)
			end
		end
	end

	if expectedProjectId ~= nil then
		if #matchingCandidates == 1 then
			local selected = matchingCandidates[1]
			rememberProjectBinding(selected, projectIdByCandidate[selected])
			return selected
		end
		if #matchingCandidates > 1 then
			SERVER.discoveryError = string.format(
				"Multiple local sync servers match project %s. Set VertigoSyncServerUrl explicitly.",
				string.sub(expectedProjectId, 1, 16)
			)
			return nil
		end
		if #healthyCandidates > 0 then
			SERVER.discoveryError = "Discovered local sync servers, but none match the expected project."
			return nil
		end
		return nil
	end

	if #healthyCandidates == 1 then
		if not canAdoptUntrusted then
			SERVER.discoveryError = "No trusted project binding yet. Click Check Connection once to trust this server."
			return nil
		end
		local selected = healthyCandidates[1]
		rememberProjectBinding(selected, projectIdByCandidate[selected])
		return selected
	end
	if #healthyCandidates > 1 then
		SERVER.discoveryError = string.format(
			"Multiple local sync servers detected (%d). Set VertigoSyncServerUrl explicitly.",
			#healthyCandidates
		)
	end
	return nil
end

-- ─── Project bootstrap helpers ──────────────────────────────────────────────

@native
local function normalizeFsPrefix(rawPath: string): string
	local normalized: string = rawPath
	if string.find(normalized, "\\", 1, true) ~= nil then
		normalized = string.gsub(normalized, "\\", "/")
	end
	while string.sub(normalized, 1, 2) == "./" do
		normalized = string.sub(normalized, 3)
	end
	while string.sub(normalized, 1, 1) == "/" do
		normalized = string.sub(normalized, 2)
	end
	while #normalized > 0 and string.sub(normalized, #normalized, #normalized) == "/" do
		normalized = string.sub(normalized, 1, #normalized - 1)
	end
	if normalized == "" then
		return ""
	end
	return normalized .. "/"
end

local function buildPathMapping(fsPath: string, instancePath: string): PathMapping?
	if fsPath == "" or instancePath == "" then
		return nil
	end

	local rawSegments: { string } = string.split(instancePath, ".")
	local instanceSegments: { string } = table.create(#rawSegments)
	for i = 1, #rawSegments do
		local segment: string = rawSegments[i]
		if segment ~= "" then
			table.insert(instanceSegments, segment)
		end
	end
	if #instanceSegments == 0 then
		return nil
	end

	local root: Instance = game
	local startIndex = 1
	local firstSegment: string = instanceSegments[1]
	if firstSegment ~= "DataModel" then
		local serviceOk, serviceOrErr = pcall(function()
			return game:GetService(firstSegment)
		end)
		if serviceOk and typeof(serviceOrErr) == "Instance" and (serviceOrErr :: Instance).Name == firstSegment then
			root = serviceOrErr :: Instance
			startIndex = 2
		end
	else
		startIndex = 2
	end

	local boundaryName: string? = nil
	local containerSegments: { string } = {}
	local segmentCount: number = #instanceSegments
	if startIndex <= segmentCount then
		boundaryName = instanceSegments[segmentCount]
		if startIndex <= segmentCount - 1 then
			containerSegments = table.create(segmentCount - startIndex)
			for i = startIndex, segmentCount - 1 do
				table.insert(containerSegments, instanceSegments[i])
			end
		end
	end

	return {
		prefix = normalizeFsPrefix(fsPath),
		root = root,
		instancePath = instancePath,
		containerSegments = containerSegments,
		boundaryName = boundaryName,
	}
end

local function sortPathMappings(mappings: { PathMapping })
	table.sort(mappings, function(a: PathMapping, b: PathMapping): boolean
		local aLen: number = #a.prefix
		local bLen: number = #b.prefix
		if aLen ~= bLen then
			return aLen > bLen
		end
		if a.prefix ~= b.prefix then
			return a.prefix < b.prefix
		end
		return a.instancePath < b.instancePath
	end)
end

local function activateDynamicPathMappings(mappings: { PathMapping })
	sortPathMappings(mappings)
	PROJECT.mappings = mappings
	PROJECT.prefixLens = table.create(#mappings)
	for i = 1, #mappings do
		PROJECT.prefixLens[i] = #mappings[i].prefix
	end
	PROJECT.mappingCount = #mappings
end

local function attachGuardRoot(root: Instance)
	if PROJECT.attachedRootGuards[root] then
		return
	end
	PROJECT.attachedRootGuards[root] = true

	root.DescendantAdded:Connect(function(descendant: Instance)
		if inSelfMutationGuard() then
			return
		end
		local managedPath = descendant:GetAttribute(CORE.MANAGED_PATH_ATTR)
		if type(managedPath) == "string" and managedPath ~= "" and resolveMapping(managedPath) ~= nil then
			managedIndex[managedPath] = descendant
			local shaAttr: any = descendant:GetAttribute(CORE.MANAGED_SHA_ATTR)
			if type(shaAttr) == "string" and shaAttr ~= "" then
				managedShaByPath[managedPath] = shaAttr
			end
		end
	end)

	root.DescendantRemoving:Connect(function(descendant: Instance)
		if inSelfMutationGuard() then
			return
		end
		local managedPath = descendant:GetAttribute(CORE.MANAGED_PATH_ATTR)
		if type(managedPath) == "string" and managedPath ~= "" and managedIndex[managedPath] == descendant then
			managedIndex[managedPath] = nil
			managedShaByPath[managedPath] = nil
		end
	end)
end

local function attachActivePathGuards()
	local mappingCount: number = #PROJECT.mappings
	for i = 1, mappingCount do
		attachGuardRoot(PROJECT.mappings[i].root)
	end
end

local function normalizeProjectPathList(rawList: any): { string }
	local normalized: { string } = {}
	if type(rawList) ~= "table" then
		return normalized
	end
	for _, rawPath in ipairs(rawList) do
		if type(rawPath) == "string" and rawPath ~= "" then
			local normalizedPath = string.gsub(rawPath, "\\", "/")
			table.insert(normalized, normalizedPath)
		end
	end
	return normalized
end

local function applyProjectPayload(payload: any): boolean
	if type(payload) ~= "table" then
		setProjectStatus("mismatch", "Malformed /project payload", nil, true)
		return false
	end
	if type(payload.name) ~= "string" or type(payload.project_id) ~= "string" or type(payload.mappings) ~= "table" then
		setProjectStatus("mismatch", "Incomplete /project payload", nil, true)
		return false
	end

	local projectName: string = payload.name
	local projectId: string = payload.project_id
	local expectedProjectId = readExpectedProjectId()
	local explicitServerBaseUrl = readConfiguredServerBaseUrl()
	if expectedProjectId ~= nil and projectId ~= expectedProjectId and explicitServerBaseUrl == nil then
		setProjectStatus(
			"mismatch",
			string.format("Server project '%s' does not match expected project", projectName),
			projectName,
			true
		)
		return false
	end
	local rawMappings: { ProjectMappingEntry } = payload.mappings
	local runtimeMappings: { PathMapping } = table.create(#rawMappings)
	local skippedMappings = 0
	for i = 1, #rawMappings do
		local entry: any = rawMappings[i]
		if type(entry) == "table" and type(entry.fs_path) == "string" and type(entry.instance_path) == "string" then
			local mapping = buildPathMapping(entry.fs_path, entry.instance_path)
			if mapping ~= nil then
				table.insert(runtimeMappings, mapping)
			else
				skippedMappings += 1
			end
		else
			skippedMappings += 1
		end
	end

	if #runtimeMappings == 0 then
		setProjectStatus("mismatch", string.format("Project '%s' exposed no usable mappings", projectName), projectName, true)
		return false
	end

	PROJECT.emitLegacyScripts = if type(payload.emit_legacy_scripts) == "boolean" then payload.emit_legacy_scripts else true
	PROJECT.projectId = projectId
	local vertigoSyncConfig: any = payload.vertigoSync
	local builderConfig: any = if type(vertigoSyncConfig) == "table" then vertigoSyncConfig.builders else nil
	local editPreviewConfig: any = if type(vertigoSyncConfig) == "table" then vertigoSyncConfig.editPreview else nil
	local function normalizeInstancePath(rawPath: any): string
		if type(rawPath) ~= "string" then
			return ""
		end
		local normalizedPath = string.gsub(rawPath, "\\", ".")
		normalizedPath = string.gsub(normalizedPath, "/", ".")
		normalizedPath = string.gsub(normalizedPath, "%.%.+", ".")
		normalizedPath = string.gsub(normalizedPath, "^%.+", "")
		normalizedPath = string.gsub(normalizedPath, "%.+$", "")
		return normalizedPath
	end
	PROJECT.builderRoots = normalizeProjectPathList(if type(builderConfig) == "table" then builderConfig.roots else nil)
	PROJECT.builderDependencyRoots =
		normalizeProjectPathList(if type(builderConfig) == "table" then builderConfig.dependencyRoots else nil)
	Runtime.clearEditPreviewWatchers()
	PROJECT.editPreview.enabled = type(editPreviewConfig) == "table" and editPreviewConfig.enabled == true
	PROJECT.editPreview.builderModulePath =
		if type(editPreviewConfig) == "table" then normalizeInstancePath(editPreviewConfig.builderModulePath) else ""
	PROJECT.editPreview.builderMethod =
		if type(editPreviewConfig) == "table" and type(editPreviewConfig.builderMethod) == "string" and editPreviewConfig.builderMethod ~= ""
			then editPreviewConfig.builderMethod
			else "Build"
	PROJECT.editPreview.watchRoots = {}
	if type(editPreviewConfig) == "table" and type(editPreviewConfig.watchRoots) == "table" then
		for _, rawRoot in ipairs(editPreviewConfig.watchRoots) do
			local normalizedRoot = normalizeInstancePath(rawRoot)
			if normalizedRoot ~= "" then
				table.insert(PROJECT.editPreview.watchRoots, normalizedRoot)
			end
		end
	end
	PROJECT.editPreview.debounceSeconds =
		if type(editPreviewConfig) == "table" and type(editPreviewConfig.debounceSeconds) == "number"
			then math.max(0.05, editPreviewConfig.debounceSeconds)
			else BUILD.DEBOUNCE_SECONDS
	PROJECT.editPreview.rootRefreshSeconds =
		if type(editPreviewConfig) == "table" and type(editPreviewConfig.rootRefreshSeconds) == "number"
			then math.max(0.25, editPreviewConfig.rootRefreshSeconds)
			else 1.0
	PROJECT.editPreview.mode =
		if type(editPreviewConfig) == "table" and type(editPreviewConfig.mode) == "string" and editPreviewConfig.mode ~= ""
			then editPreviewConfig.mode
			else "edit_only"
	Workspace:SetAttribute("VertigoSyncEditPreviewEnabled", PROJECT.editPreview.enabled)
	PROJECT.editPreview.pendingReason = nil
	PROJECT.editPreview.buildScheduled = false
	PROJECT.editPreview.buildInProgress = false
	PROJECT.editPreview.consecutiveFailures = 0
	PROJECT.editPreview.lastSkipAt = 0.0
	PROJECT.editPreview.lastSkipSignature = nil
	PROJECT.editPreview.nextRootRefreshAt = 0.0
	PROJECT.editPreview.initialBuildQueued = false
	activateDynamicPathMappings(runtimeMappings)
	PROJECT.loaded = true
	PROJECT.blocked = false
	nextProjectBootstrapAttemptAt = 0.0
	rememberProjectBinding(getServerBaseUrl(), projectId)
	bootstrapManagedIndex()
	attachActivePathGuards()

	local statusMessage: string
	if skippedMappings > 0 then
		statusMessage = string.format(
			"Loaded /project '%s' (%d mappings, %d skipped, %d builder roots, editPreview=%s)",
			projectName,
			#runtimeMappings,
			skippedMappings,
			#PROJECT.builderRoots,
			tostring(PROJECT.editPreview.enabled)
		)
	else
		statusMessage = string.format(
			"Loaded /project '%s' (%d mappings, %d builder roots, editPreview=%s)",
			projectName,
			#runtimeMappings,
			#PROJECT.builderRoots,
			tostring(PROJECT.editPreview.enabled)
		)
	end

	setProjectStatus("dynamic", statusMessage, projectName, false)
	return true
end

local function ensureProjectBootstrap(force: boolean): boolean
	if PROJECT.loaded and not force then
		return not PROJECT.blocked
	end

	local now = os.clock()
	if not force and now < nextProjectBootstrapAttemptAt then
		return false
	end

	local activeBaseUrl = getServerBaseUrl()
	local ok, payloadOrErr, statusCode = requestJson("/project", activeBaseUrl)
	if ok then
		return applyProjectPayload(payloadOrErr)
	end

	local discoveredBaseUrl = discoverServerBaseUrl(false)
	if discoveredBaseUrl ~= nil and discoveredBaseUrl ~= activeBaseUrl then
		local discoverOk, discoveredPayloadOrErr = requestJson("/project", discoveredBaseUrl)
		if discoverOk then
			return applyProjectPayload(discoveredPayloadOrErr)
		end
	end

	if SERVER.discoveryError ~= "" then
		PROJECT.loaded = false
		PROJECT.blocked = true
		nextProjectBootstrapAttemptAt = now + CORE.PROJECT_BOOTSTRAP_RETRY_SECONDS
		setProjectStatus("mismatch", SERVER.discoveryError, PROJECT.name, true)
		return false
	end

	if statusCode == 404 then
		PROJECT.loaded = false
		PROJECT.blocked = true
		nextProjectBootstrapAttemptAt = now + CORE.PROJECT_BOOTSTRAP_RETRY_SECONDS
		setProjectStatus("mismatch", string.format("Server at %s does not expose /project", getServerBaseUrl()), PROJECT.name, true)
		return false
	end

	if statusCode == 0 then
		nextProjectBootstrapAttemptAt = now + CORE.PROJECT_BOOTSTRAP_RETRY_SECONDS
		if PROJECT.mode == "bootstrapping" then
			setProjectStatus("bootstrapping", "Waiting for /project", PROJECT.name, false)
		end
		return false
	end

	PROJECT.loaded = false
	PROJECT.blocked = true
	nextProjectBootstrapAttemptAt = now + CORE.PROJECT_BOOTSTRAP_RETRY_SECONDS
	setProjectStatus("mismatch", string.format("Failed to load /project: %s", tostring(payloadOrErr)), PROJECT.name, true)
	return false
end

local function handleServerUrlChanged()
	Runtime.clearEditPreviewWatchers()
	PROJECT.loaded = false
	PROJECT.blocked = false
	PROJECT.projectId = nil
	nextProjectBootstrapAttemptAt = 0.0
	SERVER.discoveryError = ""
	resyncRequested = true
	Runtime.closeWebSocket("server_url_changed")
	setProjectStatus("bootstrapping", "Waiting for /project", nil, false)
	setStatusAttributes("disconnected", lastHash)
end

local function requestSource(path: string): (boolean, string?, string?, number, string?)
	local endpoint = "/source/" .. encodePathForRoute(path)
	local ok, rawOrErr = requestRaw(endpoint)
	if not ok then
		return false, nil, nil, 0, tostring(rawOrErr)
	end
	local raw = rawOrErr
	if raw.StatusCode < 200 or raw.StatusCode >= 300 then
		return false, nil, nil, raw.StatusCode, string.format("HTTP %d", raw.StatusCode)
	end

	local sha: string? = nil
	local headers: any = raw.Headers
	if type(headers) == "table" then
		-- Fast path: check exact casing first (server usually sends consistent casing)
		local directHit: any = headers["x-sha256"] or headers["X-Sha256"] or headers["X-SHA256"]
		if directHit ~= nil then
			sha = tostring(directHit)
		else
			-- Slow path: case-insensitive fallback
			for key: any, value: any in pairs(headers) do
				if string.lower(tostring(key)) == "x-sha256" then
					sha = tostring(value)
					break
				end
			end
		end
	end

	return true, tostring(raw.Body or ""), sha, raw.StatusCode, nil
end

local function requestSourcesBatch(paths: { string }): (boolean, SourcesContentResponse?, number, string?)
	if #paths == 0 then
		return true, {
			entries = {},
			missing = {},
		}, 200, nil
	end

	local endpoint = "/sources/content?paths=" .. HttpService:UrlEncode(table.concat(paths, ","))
	local ok, payloadOrErr, statusCode = requestJson(endpoint)
	if not ok then
		return false, nil, statusCode, tostring(payloadOrErr)
	end

	if type(payloadOrErr) ~= "table" then
		return false, nil, statusCode, "malformed /sources/content payload"
	end

	local payload = payloadOrErr :: SourcesContentResponse
	if type(payload.entries) ~= "table" then
		return false, nil, statusCode, "missing entries in /sources/content payload"
	end

	return true, payload, statusCode, nil
end

local function estimateSourceBatchEndpointChars(paths: { string }): number
	return string.len("/sources/content?paths=" .. HttpService:UrlEncode(table.concat(paths, ",")))
end

local function wouldExceedSourceBatchEndpointLimit(paths: { string }, nextPath: string): boolean
	local nextPaths: { string } = table.create(#paths + 1)
	for i = 1, #paths do
		nextPaths[i] = paths[i]
	end
	nextPaths[#paths + 1] = nextPath
	return estimateSourceBatchEndpointChars(nextPaths) > CORE.MAX_SOURCE_BATCH_ENDPOINT_CHARS
end

-- Fetch a model manifest from /model/<path> (returns JSON ModelManifest).
local function requestModelManifest(path: string): (boolean, ModelManifest?, string?, number, string?)
	local endpoint = "/model/" .. encodePathForRoute(path)
	local ok, payloadOrErr, statusCode = requestJson(endpoint)
	if not ok then
		return false, nil, nil, statusCode, tostring(payloadOrErr)
	end

	if type(payloadOrErr) ~= "table" then
		return false, nil, nil, statusCode, "malformed model manifest payload"
	end

	local manifest = payloadOrErr :: any
	if type(manifest.instances) ~= "table" then
		return false, nil, nil, statusCode, "missing instances in model manifest"
	end

	-- Normalize field names from snake_case (server) to camelCase (plugin)
	local normalizedInstances: { ModelInstance } = table.create(#manifest.instances)
	for i, entry in manifest.instances do
		normalizedInstances[i] = {
			index = entry.index,
			parentIndex = entry.parent_index, -- snake_case -> camelCase
			name = entry.name,
			className = entry.class_name, -- snake_case -> camelCase
			properties = entry.properties,
		}
	end

	local result: ModelManifest = {
		instances = normalizedInstances,
		rootCount = manifest.root_count or 0, -- snake_case -> camelCase
	}

	-- Extract sha from response headers if available
	local sha: string? = nil
	-- The /model endpoint returns JSON, so sha comes from snapshot entry
	-- We pass it through from the diff entry's expectedSha

	return true, result, sha, statusCode, nil
end

-- ─── Plugin Command Channel ─────────────────────────────────────────────────

-- @native
local function ackPluginCommand(commandId: string, success: boolean, message: string)
	pcall(function()
		local payload = HttpService:JSONEncode({
			command_id = commandId,
			success = success,
			message = message,
		})
		HttpService:RequestAsync({
			Url = getServerBaseUrl() .. "/plugin/command/ack",
			Method = "POST",
			Headers = {
				["Content-Type"] = "application/json",
			},
			Body = payload,
		})
	end)
end

-- @native
local function processPluginCommands(commands: { any })
	pluginCommandBusy = true
	Runtime.updatePluginFactAttributes()
	Runtime.reportPluginState(true)
	for _, cmd in commands do
		if type(cmd) ~= "table" or type(cmd.command) ~= "string" then
			continue
		end

		local cmdId: string = if type(cmd.id) == "string" then cmd.id else "unknown"
		local success: boolean = true
		local message: string = "ok"

		if cmd.command == "toggle_sync" then
			syncEnabled = not syncEnabled
			message = if syncEnabled then "sync enabled" else "sync disabled"
			if syncEnabled then
				resyncRequested = true
			end
		elseif cmd.command == "force_resync" then
			resyncRequested = true
			message = "resync scheduled"
		elseif cmd.command == "set_frame_budget" then
			local budgetMs: number? = if type(cmd.params) == "table" then cmd.params.budget_ms else nil
			if type(budgetMs) == "number" and budgetMs >= 1 and budgetMs <= 16 then
				adaptiveApplyBudgetSeconds = budgetMs / 1000
				message = string.format("budget set to %dms", budgetMs)
			else
				success = false
				message = "invalid budget_ms (1-16)"
			end
		elseif cmd.command == "run_builders" then
			if BUILDERS.enabled and isEditMode() then
				task.defer(runInitialBuilders)
				message = "builders scheduled"
			else
				success = false
				message = "builders not enabled or not in edit mode"
			end
		elseif cmd.command == "set_log_level" then
			local level: string? = if type(cmd.params) == "table" then cmd.params.level else nil
			if level == "quiet" then
				logLevel = 0
			elseif level == "normal" then
				logLevel = 1
			elseif level == "verbose" then
				logLevel = 2
			else
				success = false
				message = "invalid level (quiet/normal/verbose)"
			end
			if success then
				message = "log level set to " .. tostring(level)
			end
		elseif cmd.command == "time_travel" then
			local params = cmd.params
			if type(params) ~= "table" or type(params.action) ~= "string" then
				success = false
				message = "missing or invalid params.action"
			elseif params.action == "rewind" then
				if type(params.fingerprint) ~= "string" or params.fingerprint == "" then
					success = false
					message = "rewind requires a non-empty params.fingerprint"
				else
					local okFetch: boolean = true
					if not HISTORY.loaded then
						local fetchOk: boolean, fetchErr: string? = pcall(TimeTravel.fetchHistory, true)
						if not fetchOk then
							success = false
							message = "TimeTravel.fetchHistory threw: " .. tostring(fetchErr)
							okFetch = false
						elseif not HISTORY.loaded then
							success = false
							message = "failed to load history"
							okFetch = false
						end
					end
					if okFetch then
						local targetIndex: number? = nil
						for i, entry in HISTORY.entries do
							if entry.fingerprint == params.fingerprint then
								targetIndex = i
								break
							end
						end
						if targetIndex then
							local rwOk: boolean, rwErr: string? = pcall(TimeTravel.rewindToIndex, targetIndex)
							if not rwOk then
								success = false
								message = "TimeTravel.rewindToIndex threw: " .. tostring(rwErr)
							else
								message = string.format("rewound to fingerprint %s (index %d)", params.fingerprint, targetIndex)
							end
						else
							success = false
							message = "fingerprint not found in history: " .. tostring(params.fingerprint)
						end
					end
				end
			elseif params.action == "step_back" then
				if not HISTORY.loaded then
					local fetchOk: boolean = pcall(TimeTravel.fetchHistory, true)
					if not fetchOk or not HISTORY.loaded then
						success = false
						message = "failed to load history for step_back"
					end
				end
				if success then
					local sbOk: boolean, sbErr: string? = pcall(TimeTravel.stepBackward)
					if not sbOk then
						success = false
						message = "TimeTravel.stepBackward threw: " .. tostring(sbErr)
					else
						message = "stepped back"
					end
				end
			elseif params.action == "step_forward" then
				local sfOk: boolean, sfErr: string? = pcall(TimeTravel.stepForward)
				if not sfOk then
					success = false
					message = "TimeTravel.stepForward threw: " .. tostring(sfErr)
				else
					message = "stepped forward"
				end
			elseif params.action == "jump_oldest" then
				if not HISTORY.loaded then
					local fetchOk: boolean = pcall(TimeTravel.fetchHistory, true)
					if not fetchOk or not HISTORY.loaded then
						success = false
						message = "failed to load history for jump_oldest"
					end
				end
				if success then
					local joOk: boolean, joErr: string? = pcall(TimeTravel.jumpToOldest)
					if not joOk then
						success = false
						message = "TimeTravel.jumpToOldest threw: " .. tostring(joErr)
					else
						message = "jumped to oldest snapshot"
					end
				end
			elseif params.action == "resume_live" then
				local rlOk: boolean, rlErr: string? = pcall(TimeTravel.resumeLiveSync)
				if not rlOk then
					success = false
					message = "TimeTravel.resumeLiveSync threw: " .. tostring(rlErr)
				else
					message = "resumed live sync"
				end
			else
				success = false
				message = "unknown time_travel action: " .. tostring(params.action)
			end
		else
			success = false
			message = "unknown command: " .. tostring(cmd.command)
		end

		info(string.format("Command %s [%s]: %s (%s)", cmdId, cmd.command, if success then "OK" else "FAIL", message))

		-- Ack the command back to the server
		task.defer(ackPluginCommand, cmdId, success, message)
	end
	pluginCommandBusy = false
	Runtime.updatePluginFactAttributes()
	Runtime.reportPluginState(true)
end

-- ─── State Reporting (POST to server, never crashes, never logs on failure) ─

local lastStateReportAt: number = 0
local lastManagedReportAt: number = 0

function Runtime.clearEditPreviewWatchers()
	local editPreview = PROJECT.editPreview
	for instance: Instance, conn: RBXScriptConnection in pairs(editPreview.sourceConnections) do
		conn:Disconnect()
		editPreview.sourceConnections[instance] = nil
	end
	for key: string, watcher in pairs(editPreview.rootConnections) do
		for _, conn: RBXScriptConnection in ipairs(watcher.connections) do
			conn:Disconnect()
		end
		editPreview.rootConnections[key] = nil
	end
end

function Runtime.findByInstancePath(instancePath: string): Instance?
	if instancePath == "" then
		return nil
	end
	local node: Instance = game
	for segment in string.gmatch(instancePath, "[^%.]+") do
		local nextNode = node:FindFirstChild(segment)
		if nextNode == nil then
			return nil
		end
		node = nextNode
	end
	return node
end

function Runtime.canRunEditPreview(): boolean
	local mode: string = PROJECT.editPreview.mode
	if mode == "edit_only" then
		return isEditMode()
	end
	if mode == "studio_server" then
		return isEditMode() or (RunService:IsRunning() and RunService:IsServer())
	end
	return false
end

function Runtime.isEditPreviewReady(): (boolean, string?)
	if not Runtime.canRunEditPreview() then
		return false, "mode"
	end
	local suspended, suspendReason = Runtime.isEditPreviewSuspended()
	if suspended then
		return false, suspendReason or "suspended"
	end
	if Runtime.isRunAllSuiteActive() then
		return false, "runall_suite_active"
	end
	local ready, code, _message = Runtime.evaluateProjectReadiness()
	if not ready then
		return false, code
	end
	if lastHash == nil or resyncRequested or HISTORY.busy or fetchInFlight > 0 then
		return false, "snapshot_pending"
	end
	return true, nil
end

function Runtime.recordEditPreviewSkip(reason: string, context: string)
	local editPreview = PROJECT.editPreview
	local mode = describeStudioMode()
	Workspace:SetAttribute("VertigoPreviewLastSkippedReason", reason)
	Workspace:SetAttribute("VertigoPreviewLastSkippedMode", mode)
	local signature = string.format("%s|%s|%s", reason, context, mode)
	local now = os.clock()
	if editPreview.lastSkipSignature == signature and (now - editPreview.lastSkipAt) < 2.0 then
		return
	end
	editPreview.lastSkipSignature = signature
	editPreview.lastSkipAt = now
	info(string.format("Skipping preview rebuild (%s, %s): mode=%s", reason, context, mode))
end

function Runtime.shouldSkipProjectBootstrapEditPreviewBuild(): boolean
	local runAllConfig = Runtime.findByInstancePath("ServerScriptService.Tests.RunAllConfig")
	if runAllConfig == nil or not runAllConfig:IsA("ModuleScript") then
		return false
	end

	local ok, config = pcall(require, runAllConfig)
	if not ok or type(config) ~= "table" then
		return false
	end

	local specNameFilter = (config :: any).specNameFilter
	if type(specNameFilter) ~= "string" or specNameFilter == "" then
		return false
	end

	return string.find(specNameFilter, "Preview", 1, true) == nil
end

function Runtime.cancelPendingEditPreviewBuild(reason: string)
	local editPreview = PROJECT.editPreview
	editPreview.scheduleEpoch += 1
	editPreview.pendingReason = nil
	editPreview.buildScheduled = false
	Workspace:SetAttribute("VertigoPreviewLastSkippedReason", reason)
	Workspace:SetAttribute("VertigoPreviewLastSkippedMode", describeStudioMode())
end

function Runtime.resolveEditPreviewBuilderModule(): (ModuleScript?, string?)
	local builderPath: string = PROJECT.editPreview.builderModulePath
	local node = Runtime.findByInstancePath(builderPath)
	if node == nil then
		return nil, string.format("missing %s", builderPath)
	end
	if not node:IsA("ModuleScript") then
		return nil, string.format("%s is not a ModuleScript", builderPath)
	end
	return node, nil
end

function Runtime.untrackEditPreviewSource(instance: Instance)
	local conn = PROJECT.editPreview.sourceConnections[instance]
	if conn ~= nil then
		conn:Disconnect()
		PROJECT.editPreview.sourceConnections[instance] = nil
	end
end

function Runtime.trackEditPreviewSource(instance: Instance)
	local editPreview = PROJECT.editPreview
	if not instance:IsA("LuaSourceContainer") then
		return
	end
	if editPreview.sourceConnections[instance] ~= nil then
		return
	end
	editPreview.sourceConnections[instance] = instance:GetPropertyChangedSignal("Source"):Connect(function()
		local suspended = Runtime.isEditPreviewSuspended()
		if
			suspended
			or inSelfMutationGuard()
			or editPreview.buildInProgress
			or Runtime.isEditPreviewIgnoredInstance(instance)
		then
			Runtime.recordEditPreviewSkip(string.format("source_changed:%s", instance.Name), "self_mutation_guard")
			return
		end
		if not Runtime.isEditPreviewGeometryAffectingInstance(instance) then
			Runtime.recordEditPreviewSkip(string.format("source_changed:%s", instance.Name), "non_geometry_source")
			return
		end
		Runtime.scheduleEditPreviewBuild(string.format("source_changed:%s", instance:GetFullName()))
	end)
end

function Runtime.disconnectEditPreviewRoot(key: string)
	local watcher = PROJECT.editPreview.rootConnections[key]
	if watcher == nil then
		return
	end
	for _, conn: RBXScriptConnection in ipairs(watcher.connections) do
		conn:Disconnect()
	end
	for instance: Instance, _ in pairs(PROJECT.editPreview.sourceConnections) do
		if instance == watcher.root or instance:IsDescendantOf(watcher.root) then
			Runtime.untrackEditPreviewSource(instance)
		end
	end
	PROJECT.editPreview.rootConnections[key] = nil
end

function Runtime.attachEditPreviewRoot(key: string, root: Instance)
	if PROJECT.editPreview.rootConnections[key] ~= nil then
		return
	end
	local connections: { RBXScriptConnection } = {}
	for _, descendant: Instance in ipairs(root:GetDescendants()) do
		Runtime.trackEditPreviewSource(descendant)
	end
	table.insert(connections, root.DescendantAdded:Connect(function(descendant: Instance)
		Runtime.trackEditPreviewSource(descendant)
		if descendant:IsA("LuaSourceContainer") then
			local suspended = Runtime.isEditPreviewSuspended()
			if
				suspended
				or inSelfMutationGuard()
				or PROJECT.editPreview.buildInProgress
				or Runtime.isManagedMutationInstance(descendant)
				or Runtime.isEditPreviewIgnoredInstance(descendant)
			then
				Runtime.recordEditPreviewSkip(string.format("descendant_added:%s", descendant.Name), "self_mutation_guard")
				return
			end
			if not Runtime.isEditPreviewGeometryAffectingInstance(descendant) then
				Runtime.recordEditPreviewSkip(string.format("descendant_added:%s", descendant.Name), "non_geometry_source")
				return
			end
			Runtime.scheduleEditPreviewBuild(string.format("descendant_added:%s", descendant:GetFullName()))
		end
	end))
	table.insert(connections, root.DescendantRemoving:Connect(function(descendant: Instance)
		Runtime.untrackEditPreviewSource(descendant)
		if descendant:IsA("LuaSourceContainer") then
			local suspended = Runtime.isEditPreviewSuspended()
			if
				suspended
				or inSelfMutationGuard()
				or PROJECT.editPreview.buildInProgress
				or Runtime.isManagedMutationInstance(descendant)
				or Runtime.isEditPreviewIgnoredInstance(descendant)
			then
				Runtime.recordEditPreviewSkip(string.format("descendant_removed:%s", descendant.Name), "self_mutation_guard")
				return
			end
			if not Runtime.isEditPreviewGeometryAffectingInstance(descendant) then
				Runtime.recordEditPreviewSkip(string.format("descendant_removed:%s", descendant.Name), "non_geometry_source")
				return
			end
			Runtime.scheduleEditPreviewBuild(string.format("descendant_removed:%s", descendant:GetFullName()))
		end
	end))
	PROJECT.editPreview.rootConnections[key] = {
		root = root,
		connections = connections,
	}
	info(string.format("Watching editPreview root %s (%s)", key, root:GetFullName()))
end

function Runtime.refreshEditPreviewWatchRoots()
	local editPreview = PROJECT.editPreview
	if not editPreview.enabled then
		Runtime.clearEditPreviewWatchers()
		return
	end
	local suspended = Runtime.isEditPreviewSuspended()
	if suspended then
		Runtime.clearEditPreviewWatchers()
		return
	end
	for _, rootPath: string in ipairs(editPreview.watchRoots) do
		local root = Runtime.findByInstancePath(rootPath)
		local existing = editPreview.rootConnections[rootPath]
		if root == nil then
			Runtime.disconnectEditPreviewRoot(rootPath)
		elseif existing == nil or existing.root ~= root then
			Runtime.disconnectEditPreviewRoot(rootPath)
			Runtime.attachEditPreviewRoot(rootPath, root)
		end
	end
	for rootPath: string, _ in pairs(editPreview.rootConnections) do
		local keep = false
		for _, configuredRoot: string in ipairs(editPreview.watchRoots) do
			if configuredRoot == rootPath then
				keep = true
				break
			end
		end
		if not keep then
			Runtime.disconnectEditPreviewRoot(rootPath)
		end
	end
end

function Runtime.runEditPreviewBuild(reason: string)
	local editPreview = PROJECT.editPreview
	if not editPreview.enabled then
		return
	end
	local ready, context = Runtime.isEditPreviewReady()
	if not ready then
		Runtime.recordEditPreviewSkip(reason, context or "readiness")
		return
	end
	if editPreview.builderModulePath == "" then
		Workspace:SetAttribute("VertigoPreviewLastBuildError", "editPreview.builderModulePath is required")
		warnMsg("editPreview enabled, but builderModulePath is missing")
		return
	end

	editPreview.buildInProgress = true
	refreshSelfMutationGuard()
	Workspace:SetAttribute("VertigoPreviewBuildInProgress", true)
	Workspace:SetAttribute("VertigoPreviewBuildReason", reason)
	Workspace:SetAttribute("VertigoPreviewBuildMode", describeStudioMode())

	local startedAt = os.clock()
	local ok, resultOrErr = pcall(function()
		local moduleScript, resolveErr = Runtime.resolveEditPreviewBuilderModule()
		if moduleScript == nil then
			error(resolveErr or "missing editPreview builder module")
		end
		local builder = require(moduleScript)
		local builderMethod = editPreview.builderMethod
		if type(builder) ~= "table" or type((builder :: any)[builderMethod]) ~= "function" then
			error(string.format("%s.%s() is missing", editPreview.builderModulePath, builderMethod))
		end
		return ((builder :: any)[builderMethod])(builder)
	end)
	local elapsedMs = math.floor((os.clock() - startedAt) * 1000 + 0.5)

	if ok then
		editPreview.consecutiveFailures = 0
		Workspace:SetAttribute("VertigoPreviewLastBuildError", "")
		Workspace:SetAttribute("VertigoPreviewLastBuildDurationMs", elapsedMs)
		Workspace:SetAttribute("VertigoPreviewLastBuildEpoch", os.time())
		Workspace:SetAttribute("VertigoPreviewLastBuildReason", reason)
		info(string.format("Preview rebuilt (%s) in %d ms (mode=%s)", reason, elapsedMs, describeStudioMode()))
	else
		editPreview.consecutiveFailures += 1
		Workspace:SetAttribute("VertigoPreviewLastBuildError", tostring(resultOrErr))
		warnMsg(string.format(
			"Preview rebuild failed (%s) mode=%s failure=%d: %s",
			reason,
			describeStudioMode(),
			editPreview.consecutiveFailures,
			tostring(resultOrErr)
		))
		if editPreview.consecutiveFailures <= 10 then
			local retryDelay = math.min(8, 1 + editPreview.consecutiveFailures)
			task.delay(retryDelay, function()
				Runtime.scheduleEditPreviewBuild(string.format("auto_retry_%d", editPreview.consecutiveFailures))
			end)
		end
	end

	Workspace:SetAttribute("VertigoPreviewBuildInProgress", false)
	editPreview.buildInProgress = false
	refreshSelfMutationGuard()

	if editPreview.pendingReason ~= nil then
		local pendingReason = editPreview.pendingReason
		editPreview.pendingReason = nil
		Runtime.scheduleEditPreviewBuild(string.format("queued:%s", pendingReason))
	end
end

function Runtime.scheduleEditPreviewBuild(reason: string)
	local editPreview = PROJECT.editPreview
	if not editPreview.enabled then
		return
	end
	local ready, context = Runtime.isEditPreviewReady()
	if not ready then
		editPreview.pendingReason = nil
		Runtime.recordEditPreviewSkip(reason, context or "schedule")
		return
	end
	editPreview.pendingReason = reason
	if editPreview.buildScheduled then
		return
	end
	editPreview.buildScheduled = true
	local scheduleEpoch = editPreview.scheduleEpoch
	task.delay(editPreview.debounceSeconds, function()
		if editPreview.scheduleEpoch ~= scheduleEpoch then
			return
		end
		editPreview.buildScheduled = false
		local buildReason = editPreview.pendingReason or reason
		editPreview.pendingReason = nil
		local buildReady, buildContext = Runtime.isEditPreviewReady()
		if not buildReady then
			Runtime.recordEditPreviewSkip(buildReason, buildContext or "debounced")
			return
		end
		if editPreview.buildInProgress then
			editPreview.pendingReason = buildReason
			return
		end
		Runtime.runEditPreviewBuild(buildReason)
	end)
end

function Runtime.tickEditPreview()
	local editPreview = PROJECT.editPreview
	if not editPreview.enabled or not PROJECT.loaded then
		return
	end
	local now = os.clock()
	if now >= editPreview.nextRootRefreshAt then
		editPreview.nextRootRefreshAt = now + editPreview.rootRefreshSeconds
		Runtime.refreshEditPreviewWatchRoots()
	end
	local ready = Runtime.isEditPreviewReady()
	if not editPreview.initialBuildQueued and ready then
		editPreview.initialBuildQueued = true
		if Runtime.shouldSkipProjectBootstrapEditPreviewBuild() then
			Runtime.recordEditPreviewSkip("project_bootstrap", "spec_filter_non_preview")
		else
			Runtime.scheduleEditPreviewBuild("project_bootstrap")
		end
	end
end

local function decodePreviewProjectTelemetry(): ({ [string]: any }?, { [string]: any }?)
	local encoded: any = Workspace:GetAttribute("VertigoPreviewTelemetryJson")
	if type(encoded) ~= "string" or encoded == "" then
		return nil, nil
	end

	local decodeOk, decoded = pcall(function()
		return HttpService:JSONDecode(encoded)
	end)
	if not decodeOk or type(decoded) ~= "table" then
		return nil, nil
	end

	local projectFacts: any = decoded.projectFacts
	if type(projectFacts) ~= "table" then
		return decoded, nil
	end

	return decoded, projectFacts
end

function Runtime.reportPluginState(force: boolean?)
	local now: number = os.clock()
	if not force and now - lastStateReportAt < STATE_REPORT_INTERVAL_SECONDS then
		return
	end
	lastStateReportAt = now

	local queueDepth: number = math.max(#pendingQueue - pendingQueueHead + 1, 0)
	local fetchQueueDepth: number = math.max(#fetchQueue - fetchQueueHead + 1, 0)
	local previewProjectSnapshot, previewProjectFacts = decodePreviewProjectTelemetry()

	local payload: { [string]: any } = {
		plugin_version = CORE.PLUGIN_VERSION,
		preview_runtime = {
			studio_connected = true,
			plugin_attached = true,
			project_loaded = PROJECT.loaded,
			sync_status = currentStatus,
			connection = {
				sync_status = currentStatus,
				ws_connected = wsConnected,
				has_ever_connected = hasEverConnected,
			},
		},
		preview_project = previewProjectFacts,
		preview_project_snapshot = previewProjectSnapshot,
		connection = {
			sync_status = currentStatus,
			transport_mode = transportMode,
			ws_connected = wsConnected,
			has_ever_connected = hasEverConnected,
			reconnect_attempt = connectionReconnectAttempt,
		},
		project_loaded = PROJECT.loaded,
		snapshot_state = {
			state = if HISTORY.fetchFailed
				then "fetch_failed"
				elseif HISTORY.busy
				then "apply_in_progress"
				elseif HISTORY.active
				then "historical"
				elseif resyncRequested
				then "resync_requested"
				elseif fetchInFlight > 0
				then "fetching"
				elseif PROJECT.loaded
				then "live"
				else "project_pending",
			hash = lastHash,
			history_loaded = HISTORY.loaded,
			history_active = HISTORY.active,
			history_busy = HISTORY.busy,
			fetch_failed = HISTORY.fetchFailed,
			fetch_in_flight = fetchInFlight,
			fetch_queue_depth = fetchQueueDepth,
			pending_queue_depth = queueDepth,
			resync_requested = resyncRequested,
		},
		snapshot_apply_in_progress = HISTORY.busy or fetchInFlight > 0,
		plugin_command_busy = pluginCommandBusy,
	}

	pcall(function()
		local jsonBody: string = HttpService:JSONEncode(payload)
		local raw = HttpService:RequestAsync({
			Url = getServerBaseUrl() .. "/plugin/state",
			Method = "POST",
			Headers = {
				["Content-Type"] = "application/json",
			},
			Body = jsonBody,
		})
		-- Server returns 200 with pending commands, or 204 with no body.
		if raw.StatusCode == 200 then
			local decodeOk, body = pcall(function()
				return HttpService:JSONDecode(raw.Body)
			end)
			if decodeOk and type(body) == "table" and type(body.commands) == "table" then
				processPluginCommands(body.commands)
			end
		end
	end)
end

function Runtime.reportPluginManaged()
	local now: number = os.clock()
	if now - lastManagedReportAt < MANAGED_REPORT_INTERVAL_SECONDS then
		return
	end
	lastManagedReportAt = now

	local entries: { { path: string, sha256: string, class: string } } = {}
	local totalBytes: number = 0
	for path: string, inst: Instance in managedIndex do
		local sha: string = managedShaByPath[path] or ""
		local className: string = inst.ClassName
		table.insert(entries, {
			path = path,
			sha256 = sha,
			class = className,
		})
		-- Approximate byte count from source length
		if inst:IsA("LuaSourceContainer") then
			local srcLen: number = #((inst :: any).Source or "")
			totalBytes += srcLen
		end
	end

	local payload: { [string]: any } = {
		entries = entries,
		total_count = #entries,
		total_bytes = totalBytes,
	}

	pcall(function()
		local jsonBody: string = HttpService:JSONEncode(payload)
		HttpService:RequestAsync({
			Url = getServerBaseUrl() .. "/plugin/managed",
			Method = "POST",
			Headers = {
				["Content-Type"] = "application/json",
			},
			Body = jsonBody,
		})
	end)
end

-- ─── Instance Pool ──────────────────────────────────────────────────────────

function Runtime.initInstancePool()
	local classCount: number = #POOL.CLASSES
	for i = 1, classCount do
		local className: string = POOL.CLASSES[i]
		local pool: { Instance } = table.create(POOL.SIZE)
		for j = 1, POOL.SIZE do
			local inst: Instance = Instance.new(className)
			inst.Parent = nil
			pool[j] = inst
		end
		instancePool[className] = pool
	end
end

@native
function Runtime.poolGet(className: string): Instance
	local pool: { Instance }? = instancePool[className]
	if pool ~= nil and #pool > 0 then
		local inst: Instance = table.remove(pool) :: Instance
		return inst
	end
	return Instance.new(className)
end

@native
function Runtime.poolReturn(inst: Instance)
	local pool: { Instance }? = instancePool[inst.ClassName]
	if pool ~= nil and #pool < POOL.SIZE then
		inst.Parent = nil
		inst.Name = ""
		table.insert(pool, inst)
	else
		inst:Destroy()
	end
end

-- ─── Queue helpers ──────────────────────────────────────────────────────────

@native
function Runtime.resetTransientSyncState()
	pendingOps = {}
	pendingQueue = {}
	pendingQueueHead = 1
	fetchQueue = {}
	fetchQueueHead = 1
	fetchQueuedEpoch = {}
	inflightFetchEpoch = {}
	readySources = {}
	readyModels = {}
	modelApplyQueues = {}
	modelApplyQueueHeads = {}
	modelBuildLookup = {}
	modelApplyActive = {}
	fetchInFlight = 0
end

@native
function Runtime.compactPathQueueIfNeeded()
	if pendingQueueHead > 1024 and pendingQueueHead > #pendingQueue then
		pendingQueue = {}
		pendingQueueHead = 1
	end
end

@native
function Runtime.enqueuePath(path: string)
	local queueDepth = #pendingQueue - pendingQueueHead + 1
	local queueLimit = SETTINGS.applyQueueLimit
	if queueLimit > 0 and queueDepth > queueLimit then
		warn("[VertigoSync] Apply queue overflow — forcing resync")
		Runtime.resetTransientSyncState()
		setStatusAttributes("error", lastHash)
		resyncRequested = true
		return
	end
	local op: PendingOp? = pendingOps[path]
	if op and not op.queued then
		op.queued = true
		table.insert(pendingQueue, path)
	elseif op == nil then
		throttledLog("queue_inconsistency", string.format("Internal queue inconsistency for path '%s'", path), true)
	end
end

@native
function Runtime.popPendingPath(): string?
	local queueLen: number = #pendingQueue
	while pendingQueueHead <= queueLen do
		local path: string = pendingQueue[pendingQueueHead]
		pendingQueue[pendingQueueHead] = ""
		pendingQueueHead += 1
		if path ~= "" then
			Runtime.compactPathQueueIfNeeded()
			return path
		end
	end
	Runtime.compactPathQueueIfNeeded()
	return nil
end

@native
function Runtime.compactFetchQueueIfNeeded()
	if fetchQueueHead > 1024 and fetchQueueHead > #fetchQueue then
		fetchQueue = {}
		fetchQueueHead = 1
	end
end

@native
function Runtime.pushFetchTask(path: string, epoch: number)
	local queuedEpoch: number? = fetchQueuedEpoch[path]
	if queuedEpoch ~= nil and queuedEpoch >= epoch then
		return
	end
	local inflightEpoch: number? = inflightFetchEpoch[path]
	if inflightEpoch ~= nil and inflightEpoch >= epoch then
		return
	end

	fetchQueuedEpoch[path] = epoch
	table.insert(fetchQueue, {
		path = path,
		epoch = epoch,
	})
end

@native
function Runtime.popFetchTask(): FetchTask?
	local queueLen: number = #fetchQueue
	while fetchQueueHead <= queueLen do
		local taskItem: FetchTask? = fetchQueue[fetchQueueHead]
		fetchQueue[fetchQueueHead] = nil
		fetchQueueHead += 1
		if taskItem ~= nil then
			Runtime.compactFetchQueueIfNeeded()
			return taskItem
		end
	end
	Runtime.compactFetchQueueIfNeeded()
	return nil
end

-- ─── Path resolution / mapping ──────────────────────────────────────────────

resolveMapping = function(filePath: string): (PathMapping?, string?)
	local mappingCount: number = #PROJECT.mappings
	for i = 1, mappingCount do
		local prefixLen: number = PROJECT.prefixLens[i]
		local mapping: PathMapping = PROJECT.mappings[i]
		if prefixLen == 0 or string.sub(filePath, 1, prefixLen) == mapping.prefix then
			local remainder: string = string.sub(filePath, prefixLen + 1)
			return mapping, remainder
		end
	end
	return nil, nil
end

@native
function Runtime.stripExtension(name: string): string
	-- Avoid pattern matching in hot path; check suffix directly
	local nameLen: number = #name
	if nameLen >= 5 and string.sub(name, nameLen - 4) == ".luau" then
		return string.sub(name, 1, nameLen - 5)
	elseif nameLen >= 4 and string.sub(name, nameLen - 3) == ".lua" then
		return string.sub(name, 1, nameLen - 4)
	elseif nameLen >= 6 and string.sub(name, nameLen - 5) == ".jsonc" then
		return string.sub(name, 1, nameLen - 6)
	elseif nameLen >= 5 and string.sub(name, nameLen - 4) == ".json" then
		return string.sub(name, 1, nameLen - 5)
	elseif nameLen >= 4 and string.sub(name, nameLen - 3) == ".txt" then
		return string.sub(name, 1, nameLen - 4)
	elseif nameLen >= 4 and string.sub(name, nameLen - 3) == ".csv" then
		return string.sub(name, 1, nameLen - 4)
	end
	return name
end

@native
function Runtime.isInitFile(name: string): boolean
	return INIT_FILES[name] == true
end

@native
function Runtime.classForFile(fileName: string): string
	local mapped: string? = CLASS_MAP[fileName]
	if mapped ~= nil then
		if not PROJECT.emitLegacyScripts and mapped == "LocalScript" then
			return "Script"
		end
		return mapped
	end
	-- Non-init files: check suffix for server/client hint
	if string.find(fileName, ".server.", 1, true) then
		return "Script"
	elseif string.find(fileName, ".client.", 1, true) then
		return if PROJECT.emitLegacyScripts then "LocalScript" else "Script"
	end
	-- Extended file type support
	if string.find(fileName, ".jsonc", 1, true) then
		return "ModuleScript"
	end
	if string.find(fileName, ".json", 1, true) then
		return "ModuleScript"
	end
	if string.find(fileName, ".txt", 1, true) then
		return "StringValue"
	end
	if string.find(fileName, ".csv", 1, true) then
		return "LocalizationTable"
	end
	return "ModuleScript"
end

function Runtime.runContextForPath(filePath: string): Enum.RunContext?
	if PROJECT.emitLegacyScripts then
		return nil
	end

	local normalized: string = filePath
	if string.find(normalized, "\\", 1, true) ~= nil then
		normalized = string.gsub(normalized, "\\", "/")
	end
	local segments: { string } = string.split(normalized, "/")
	local fileName: string = segments[#segments] or normalized

	if fileName == "init.server.luau" or fileName == "init.server.lua" then
		return Enum.RunContext.Server
	end
	if fileName == "init.client.luau" or fileName == "init.client.lua" then
		return Enum.RunContext.Client
	end
	if string.find(fileName, ".server.", 1, true) ~= nil then
		return Enum.RunContext.Server
	end
	if string.find(fileName, ".client.", 1, true) ~= nil then
		return Enum.RunContext.Client
	end
	return nil
end

@native
function Runtime.fileTypeForPath(filePath: string): string?
	local pathLen: number = #filePath
	-- Check .rbxmx (6 chars) BEFORE .rbxm (5 chars) since .rbxm is a suffix of .rbxmx
	if pathLen >= 6 and string.sub(filePath, pathLen - 5) == ".rbxmx" then
		return "rbxmx"
	end
	if pathLen >= 5 and string.sub(filePath, pathLen - 4) == ".rbxm" then
		return "rbxm"
	end
	if pathLen >= 6 and string.sub(filePath, pathLen - 5) == ".jsonc" then
		return "jsonc"
	end
	if pathLen >= 5 and string.sub(filePath, pathLen - 4) == ".json" then
		return "json"
	end
	if pathLen >= 4 and string.sub(filePath, pathLen - 3) == ".txt" then
		return "txt"
	end
	if pathLen >= 4 and string.sub(filePath, pathLen - 3) == ".csv" then
		return "csv"
	end
	return nil
end

@native
function Runtime.isBinaryModelType(fileType: string?): boolean
	return fileType == "rbxm" or fileType == "rbxmx"
end

@native
function Runtime.parseRelativePath(relativePath: string): ({ string }, string, string)
	local rawSegments: { string } = string.split(relativePath, "/")
	-- Filter empty segments from leading/trailing/double slashes
	local segments: { string } = table.create(#rawSegments)
	local segCount: number = 0
	for i = 1, #rawSegments do
		local seg: string = rawSegments[i]
		if seg ~= "" then
			segCount += 1
			segments[segCount] = seg
		end
	end

	if segCount == 0 then
		return {}, "ModuleScript", "Unknown"
	end

	local fileName: string = segments[segCount]
	if Runtime.isInitFile(fileName) then
		local className: string = Runtime.classForFile(fileName)
		segments[segCount] = nil :: any
		segCount -= 1
		if segCount == 0 then
			return {}, className, ""
		end
		local instanceName: string = segments[segCount]
		segments[segCount] = nil :: any
		return segments, className, instanceName
	end

	local className: string = Runtime.classForFile(fileName)
	local instanceName: string = Runtime.stripExtension(fileName)
	segments[segCount] = nil :: any
	return segments, className, instanceName
end

function Runtime.ensureContainer(parent: Instance, childName: string): Instance
	local existing = parent:FindFirstChild(childName)
	if existing ~= nil then
		return existing
	end

	refreshSelfMutationGuard()
	local folder = Runtime.poolGet("Folder")
	folder.Name = childName
	folder.Parent = parent
	return folder
end

@native
function Runtime.ensureAncestors(root: Instance, segments: { string }): Instance
	local current: Instance = root
	local segCount: number = #segments
	for i = 1, segCount do
		current = Runtime.ensureContainer(current, segments[i])
	end
	return current
end

-- Ensure or create an instance of the given class under parent with the given name.
-- If an existing child has the wrong class, replace it.
function Runtime.ensureOrCreate(parent: Instance, instanceName: string, className: string): Instance
	local existing: Instance? = parent:FindFirstChild(instanceName)
	if existing ~= nil then
		if existing.ClassName == className then
			return existing
		end
		-- Wrong class: replace
		refreshSelfMutationGuard()
		local replacement: Instance = Runtime.poolGet(className)
		replacement.Name = instanceName
		for _, child in ipairs(existing:GetChildren()) do
			child.Parent = replacement
		end
		replacement.Parent = parent
		Runtime.poolReturn(existing)
		return replacement
	end
	refreshSelfMutationGuard()
	local created: Instance = Runtime.poolGet(className)
	created.Name = instanceName
	created.Parent = parent
	return created
end

@native
function Runtime.findBoundaryRoot(mapping: PathMapping): Instance?
	local current: Instance = mapping.root
	local containerCount: number = #mapping.containerSegments
	for i = 1, containerCount do
		local nextChild: Instance? = current:FindFirstChild(mapping.containerSegments[i])
		if nextChild == nil then
			return nil
		end
		current = nextChild
	end
	if mapping.boundaryName ~= nil and mapping.boundaryName ~= "" then
		return current:FindFirstChild(mapping.boundaryName)
	end
	return current
end

function Runtime.visitManagedScope(scopeRoot: Instance, callback: (Instance) -> ())
	callback(scopeRoot)
	local descendants: { Instance } = scopeRoot:GetDescendants()
	for i = 1, #descendants do
		callback(descendants[i])
	end
end

@native
function Runtime.resolveTarget(filePath: string): (Instance?, string?, string?, Instance?)
	local mapping, remainder = resolveMapping(filePath)
	if mapping == nil or remainder == nil then
		return nil, nil, nil, nil
	end

	local segments, className, instanceName = Runtime.parseRelativePath(remainder)
	local boundaryParent: Instance = mapping.root
	if #mapping.containerSegments > 0 then
		boundaryParent = Runtime.ensureAncestors(mapping.root, mapping.containerSegments)
	end

	if instanceName == "" then
		if mapping.boundaryName == nil or mapping.boundaryName == "" then
			return nil, nil, nil, nil
		end
		return boundaryParent, mapping.boundaryName, className, boundaryParent
	end

	local boundary: Instance = boundaryParent
	if mapping.boundaryName ~= nil and mapping.boundaryName ~= "" then
		boundary = Runtime.ensureContainer(boundaryParent, mapping.boundaryName)
	end
	local parent = Runtime.ensureAncestors(boundary, segments)
	return parent, instanceName, className, boundary
end

function Runtime.replaceInstanceClassPreservingChildren(existing: Instance, className: string): Instance
	refreshSelfMutationGuard()
	local replacement = Runtime.poolGet(className)
	replacement.Name = existing.Name

	for _, child in ipairs(existing:GetChildren()) do
		child.Parent = replacement
	end

	local parent = existing.Parent
	Runtime.poolReturn(existing)
	replacement.Parent = parent
	return replacement
end

function Runtime.cleanupEmptyAncestors(parent: Instance?, boundary: Instance?)
	local current = parent
	while current ~= nil and current ~= boundary do
		if not current:IsA("Folder") then
			break
		end
		if #current:GetChildren() > 0 then
			break
		end
		local nextParent = current.Parent
		refreshSelfMutationGuard()
		Runtime.poolReturn(current)
		current = nextParent
	end
end

-- ─── Managed index bootstrap ────────────────────────────────────────────────

bootstrapManagedIndex = function()
	managedIndex = {}
	managedShaByPath = {}

	local mappingCount: number = #PROJECT.mappings
	for i = 1, mappingCount do
		local mapping: PathMapping = PROJECT.mappings[i]
		local boundaryRoot: Instance? = Runtime.findBoundaryRoot(mapping)
		if boundaryRoot ~= nil then
			Runtime.visitManagedScope(boundaryRoot, function(descendant: Instance)
				local pathAttr: any = descendant:GetAttribute(CORE.MANAGED_PATH_ATTR)
				if type(pathAttr) == "string" and pathAttr ~= "" and resolveMapping(pathAttr) ~= nil then
					managedIndex[pathAttr] = descendant
					local shaAttr: any = descendant:GetAttribute(CORE.MANAGED_SHA_ATTR)
					if type(shaAttr) == "string" and shaAttr ~= "" then
						managedShaByPath[pathAttr] = shaAttr
					end
				end
			end)
		end
	end
end

-- ─── Meta property application ──────────────────────────────────────────────

function Runtime.applyMeta(inst: Instance, meta: EntryMeta?)
	if meta == nil then
		return
	end
	local properties: { [string]: any }? = meta.properties
	if properties ~= nil then
		for propName: string, propValue: any in pairs(properties) do
			local success: boolean = pcall(function()
				(inst :: any)[propName] = propValue
			end)
			if not success then
				throttledLog("meta_prop_" .. propName, string.format("Failed to set property %s on %s", propName, inst:GetFullName()), true)
			end
		end
	end
	if meta.attributes then
		for attrName: string, attrValue: any in meta.attributes do
			local attrOk: boolean = pcall(function()
				inst:SetAttribute(attrName, attrValue)
			end)
			if not attrOk then
				throttledLog("meta_attr_" .. attrName, string.format("Failed to set attribute %s on %s", attrName, inst:GetFullName()), true)
			end
		end
	end
end

-- ─── Binary Model Instance Pipeline ─────────────────────────────────────────

-- Destroy all instances previously created from a binary model at the given path.
-- Uses CORE.MANAGED_PATH_ATTR to find root instances, then destroys the entire subtree.
function Runtime.cleanupModelInstances(path: string)
	-- Find all root instances tagged with this model path
	local mappingCount: number = #PROJECT.mappings
	for i = 1, mappingCount do
		local mapping: PathMapping = PROJECT.mappings[i]
		local boundaryRoot: Instance? = Runtime.findBoundaryRoot(mapping)
		if boundaryRoot ~= nil then
			local pendingDestroy: { Instance } = {}
			Runtime.visitManagedScope(boundaryRoot, function(descendant: Instance)
				local pathAttr: any = descendant:GetAttribute(CORE.MANAGED_PATH_ATTR)
				if pathAttr == path then
					table.insert(pendingDestroy, descendant)
				end
			end)
			for j = #pendingDestroy, 1, -1 do
				refreshSelfMutationGuard()
				pendingDestroy[j]:Destroy()
			end
		end
	end
	-- Clean up tracking state
	managedIndex[path] = nil
	managedShaByPath[path] = nil
	modelApplyQueues[path] = nil
	modelApplyQueueHeads[path] = nil
	modelBuildLookup[path] = nil
	modelApplyActive[path] = nil
end

-- Stage a model manifest as individual frame-budgeted instance creation ops.
-- Instances are ordered topologically (parents before children) by the server,
-- so we can process them in index order.
function Runtime.stageModelManifest(path: string, manifest: ModelManifest, epoch: number, sha256: string?)
	-- Clean up any existing model instances at this path
	Runtime.cleanupModelInstances(path)

	local instanceCount: number = #manifest.instances
	if instanceCount == 0 then
		return
	end

	-- Build the ops queue (already topologically ordered by server)
	local ops: { ModelApplyOp } = table.create(instanceCount)
	for i = 1, instanceCount do
		ops[i] = {
			manifestPath = path,
			manifestEpoch = epoch,
			instanceIndex = i,
			entry = manifest.instances[i],
		}
	end

	-- Store the queue and initialize tracking
	modelApplyQueues[path] = ops
	modelApplyQueueHeads[path] = 1
	modelBuildLookup[path] = {}
	modelApplyActive[path] = true

	-- Store expected sha for managed index tracking
	if sha256 ~= nil and sha256 ~= "" then
		managedShaByPath[path] = sha256
	end

	-- Enqueue a sentinel op so the apply loop picks this up
	opEpoch += 1
	local sentinelEpoch: number = opEpoch
	pendingOps[path] = {
		action = "model_apply",
		epoch = sentinelEpoch,
		retries = 0,
		queued = false,
		expectedSha = sha256,
	}
	Runtime.enqueuePath(path)

	info(string.format("Staged model manifest: %s (%d instances, %d roots)", path, instanceCount, manifest.rootCount))
end

-- Process one model instance creation op from the staged queue.
-- Returns true if an instance was created, false if the queue is exhausted.
@native
function Runtime.processOneModelOp(path: string): boolean
	local ops: { ModelApplyOp }? = modelApplyQueues[path]
	if ops == nil then
		return false
	end

	local head: number = modelApplyQueueHeads[path] or 1
	local opCount: number = #ops
	if head > opCount then
		-- Queue exhausted — model is fully applied
		modelApplyActive[path] = nil
		modelApplyQueues[path] = nil
		modelApplyQueueHeads[path] = nil
		-- Keep modelBuildLookup[path] alive for potential future reference
		return false
	end

	local op: ModelApplyOp = ops[head]
	modelApplyQueueHeads[path] = head + 1

	local entry: ModelInstance = op.entry
	local lookup: { [number]: Instance } = modelBuildLookup[path] or {}

	-- Create the instance using the pool
	local inst: Instance = Runtime.poolGet(entry.className)
	inst.Name = entry.name

	-- Apply properties via pcall (same pattern as .meta.json property application)
	if entry.properties ~= nil then
		for propName: string, propValue: any in pairs(entry.properties) do
			pcall(function()
				(inst :: any)[propName] = propValue
			end)
		end
	end

	-- Parent the instance
	if entry.parentIndex ~= nil then
		local parentInst: Instance? = lookup[entry.parentIndex + 1] -- Lua 1-indexed
		if parentInst ~= nil then
			refreshSelfMutationGuard()
			inst.Parent = parentInst
		else
			-- Parent not yet created (shouldn't happen with topological order)
			throttledLog("model_parent_miss_" .. path, string.format("Model parent index %d not found for %s in %s", entry.parentIndex, entry.name, path), true)
			refreshSelfMutationGuard()
			inst.Parent = Workspace -- fallback
		end
	else
		-- Root instance — resolve the target parent from path mapping
		local parent, _instanceName, _className, _boundary = Runtime.resolveTarget(path)
		if parent ~= nil then
			refreshSelfMutationGuard()
			inst.Parent = parent
			-- Tag root instances for managed tracking
			inst:SetAttribute(CORE.MANAGED_PATH_ATTR, path)
			if managedShaByPath[path] ~= nil then
				inst:SetAttribute(CORE.MANAGED_SHA_ATTR, managedShaByPath[path])
			end
			-- Track the first root in managed index for SHA tracking
			if managedIndex[path] == nil then
				managedIndex[path] = inst
			end
		else
			throttledLog("model_resolve_fail_" .. path, string.format("Cannot resolve parent for model root: %s", path), true)
			inst:Destroy()
			return true -- consumed a tick, even though it failed
		end
	end

	-- Store in build lookup (1-indexed: entry.index is 0-based from server)
	lookup[entry.index + 1] = inst
	modelBuildLookup[path] = lookup

	return true
end

-- ─── Builder Execution (defined before applyWrite) ──────────────────────────

-- Require-cache invalidation: clone the ModuleScript, require the clone, then
-- destroy the clone only after any returned Build()/Init() methods finish.
-- This keeps `script.Parent` and relative requires alive for the duration of
-- builder execution.
function Runtime.requireFresh(scriptInstance: ModuleScript): (boolean, any, ModuleScript?)
	local clone: ModuleScript = scriptInstance:Clone()
	clone.Name = scriptInstance.Name .. "_BuilderClone"
	clone.Parent = scriptInstance.Parent
	local ok: boolean, result: any = pcall(require, clone)
	if not ok then
		clone:Destroy()
		return ok, result, nil
	end
	return ok, result, clone
end

-- Scan builder source for require() calls and populate the dependency map.
-- Uses string.find (plain) to locate `require(` tokens, then extracts the
-- path segments to map shared module paths back to builder paths.
function Runtime.computeBuilderDependencies(builderPath: string, scriptInstance: LuaSourceContainer)
	local source: string = scriptInstance.Source or ""
	if source == "" then
		return
	end

	-- Look for require( patterns and extract approximate dependency paths
	local searchStart: number = 1
	while true do
		local reqStart: number? = string.find(source, "require(", searchStart, true)
		if reqStart == nil then
			break
		end
		searchStart = reqStart + 8

		-- Extract the require argument up to the closing paren
		local closeParenPos: number? = string.find(source, ")", searchStart, true)
		if closeParenPos == nil then
			break
		end
		local requireArg: string = string.sub(source, searchStart, closeParenPos - 1)

		-- Check if this require references a dependency path
		-- Patterns like: script.Parent.Parent.Elements.WorldElements
		-- or: Shared.Config.Creatures, Shared.Util.Noise3D
		local depPathCount: number = #PROJECT.builderDependencyRoots
		for i = 1, depPathCount do
			local depPrefix: string = PROJECT.builderDependencyRoots[i]
			-- Convert path prefix to module-name segments for matching
			-- e.g. "src/Server/World/Elements/" -> "Elements"
			-- e.g. "src/Shared/Config/" -> "Config"
			-- e.g. "src/Shared/Util/" -> "Util"
			local segments: { string } = string.split(depPrefix, "/")
			local lastSegment: string = ""
			for j = #segments, 1, -1 do
				if segments[j] ~= "" then
					lastSegment = segments[j]
					break
				end
			end

			if lastSegment ~= "" and string.find(requireArg, lastSegment, 1, true) ~= nil then
				-- This builder depends on something under this dependency path
				-- Register all files under this prefix as potential triggers
				if BUILDERS.dependencyMap[depPrefix] == nil then
					BUILDERS.dependencyMap[depPrefix] = {}
				end
				BUILDERS.dependencyMap[depPrefix][builderPath] = true
			end
		end

		searchStart = closeParenPos + 1
	end
end

function Runtime.mergeBuilderReturnedInstances(captured: { Instance }, buildReturn: any)
	if typeof(buildReturn) == "Instance" then
		table.insert(captured, buildReturn)
		return
	end
	if type(buildReturn) ~= "table" then
		return
	end
	for _, candidate: any in ipairs(buildReturn) do
		if typeof(candidate) == "Instance" then
			table.insert(captured, candidate)
		end
	end
end

function Runtime.builderFailure(path: string, stage: string, err: any)
	warnMsg(string.format("Builder %s failed for %s: %s", stage, path, tostring(err)))
end

function Runtime.builderOutputTag(path: string, sourceHash: string): string
	local segments: { string } = string.split(path, "/")
	local joined: string = table.concat(segments, "_")
	local parts: { string } = string.split(joined, ".")
	local sanitized: string = table.concat(parts, "_")
	return "BuilderOutput_" .. sanitized .. "_" .. string.sub(sourceHash, 1, 8)
end

function Runtime.enqueueBuilderPath(path: string)
	if BUILDERS.queuedSet[path] then
		return
	end
	BUILDERS.queuedSet[path] = true
	table.insert(BUILDERS.queue, path)
	updateBuilderPerfAttributes()
end

function Runtime.hasQueuedBuilders(): boolean
	return BUILDERS.queueHead <= #BUILDERS.queue
end

function Runtime.dequeueBuilderPath(): string?
	if BUILDERS.queueHead > #BUILDERS.queue then
		BUILDERS.queue = {}
		BUILDERS.queueHead = 1
		return nil
	end

	local path: string = BUILDERS.queue[BUILDERS.queueHead]
	BUILDERS.queue[BUILDERS.queueHead] = nil :: any
	BUILDERS.queueHead += 1
	BUILDERS.queuedSet[path] = nil

	if BUILDERS.queueHead > #BUILDERS.queue then
		BUILDERS.queue = {}
		BUILDERS.queueHead = 1
	end

	updateBuilderPerfAttributes()

	return path
end

-- Schedule a debounced batch of dirty builder re-executions
function Runtime.executeBuilder(path: string, scriptInstance: LuaSourceContainer): string
	local sourceHash: string = scriptInstance:GetAttribute(CORE.MANAGED_SHA_ATTR) or "unknown"
	local outputTag: string = Runtime.builderOutputTag(path, sourceHash)
	local forceRebuild: boolean = BUILDERS.forceSet[path] == true

	-- Check if output already exists with matching hash
	local existingOutputs: { Instance } = CollectionService:GetTagged(BUILDERS.outputTags[path] or "")
	if not forceRebuild and #existingOutputs > 0 and BUILDERS.outputTags[path] == outputTag then
		Runtime.computeBuilderDependencies(path, scriptInstance)
		return "skipped"
	end
	BUILDERS.forceSet[path] = nil

	-- Clean up old output
	if BUILDERS.outputTags[path] ~= nil then
		for _, old: Instance in CollectionService:GetTagged(BUILDERS.outputTags[path]) do
			old:Destroy()
		end
	end

	-- Capture new children via ChildAdded listener
	local newChildren: { Instance } = {}
	local conn: RBXScriptConnection = Workspace.ChildAdded:Connect(function(child: Instance)
		table.insert(newChildren, child)
	end)

	-- Execute the builder module with fresh require to bypass cache
	local execOk: boolean, result: any, builderClone: ModuleScript? = Runtime.requireFresh(scriptInstance :: ModuleScript)
	local buildReturn: any = nil

	if not execOk then
		conn:Disconnect()
		if builderClone ~= nil then
			builderClone:Destroy()
		end
		Runtime.builderFailure(path, "require", result)
		return "failed"
	end

	if type(result) == "table" then
		local backgroundBuild: boolean = result.BackgroundBuild == true
		-- If the builder returns a table with :Init() and :Build(), call them
		if type(result.Init) == "function" then
			local initOk: boolean, initErr: any = pcall(result.Init, result)
			if not initOk then
				Runtime.builderFailure(path, "init", initErr)
			end
		end
		if type(result.Build) == "function" then
			local buildOk: boolean, returned: any = pcall(result.Build, result)
			if buildOk then
				buildReturn = returned
			else
				conn:Disconnect()
				if builderClone ~= nil then
					builderClone:Destroy()
				end
				Runtime.builderFailure(path, "build", returned)
				return "failed"
			end
		end

		if backgroundBuild then
			conn:Disconnect()
			if builderClone ~= nil then
				builderClone:Destroy()
			end
			Runtime.mergeBuilderReturnedInstances(newChildren, buildReturn)

			for _, child: Instance in newChildren do
				CollectionService:AddTag(child, outputTag)
				child:SetAttribute("VertigoBuildHash", sourceHash)
				child:SetAttribute("VertigoBuildPath", path)
			end

			Runtime.computeBuilderDependencies(path, scriptInstance)
			BUILDERS.sources[path] = sourceHash
			BUILDERS.outputTags[path] = outputTag
			Workspace:SetAttribute("VertigoBuilderLastRebuild", os.clock())
			Workspace:SetAttribute("VertigoBuilderLastRebuildPath", path)

			info(string.format("Builder scheduled: %s (hash=%s, captured=%d)", path, string.sub(sourceHash, 1, 8), #newChildren))
			return "executed"
		end
	else
		conn:Disconnect()
		Runtime.builderFailure(path, "require", "builder module must return a table")
		if builderClone ~= nil then
			builderClone:Destroy()
		end
		return "failed"
	end

	-- Wait for builder output to quiesce (3 frames with no new children)
	local quietFrames = 0
	while quietFrames < 3 do
		local countBefore = #newChildren
		task.wait()
		if #newChildren == countBefore then
			quietFrames += 1
		else
			quietFrames = 0
		end
	end

	conn:Disconnect()
	if builderClone ~= nil then
		builderClone:Destroy()
	end
	Runtime.mergeBuilderReturnedInstances(newChildren, buildReturn)

	-- Tag all captured output and persist build hash for cross-session stale detection
	for _, child: Instance in newChildren do
		CollectionService:AddTag(child, outputTag)
		child:SetAttribute("VertigoBuildHash", sourceHash)
		child:SetAttribute("VertigoBuildPath", path)
	end

	-- Compute dependencies for this builder
	Runtime.computeBuilderDependencies(path, scriptInstance)

	-- Update tracking state
	BUILDERS.sources[path] = sourceHash
	BUILDERS.outputTags[path] = outputTag

	-- Signal runtime to refresh tag caches
	Workspace:SetAttribute("VertigoBuilderLastRebuild", os.clock())
	Workspace:SetAttribute("VertigoBuilderLastRebuildPath", path)

	info(string.format("Builder executed: %s (hash=%s, captured=%d)", path, string.sub(sourceHash, 1, 8), #newChildren))
	return "executed"
end

function Runtime.scheduleBuilderPump()
	if BUILDERS.pumpScheduled or BUILDERS.pumpActive then
		return
	end
	BUILDERS.pumpScheduled = true
	task.defer(function()
		BUILDERS.pumpScheduled = false
		if BUILDERS.pumpActive then
			return
		end

		BUILDERS.pumpActive = true
		updateBuilderPerfAttributes()
		local sliceStart: number = os.clock()
		local processed: number = 0

		while processed < BUILD.MAX_BUILDERS_PER_SLICE and os.clock() - sliceStart < BUILD.FRAME_BUDGET_SECONDS do
			local path: string? = Runtime.dequeueBuilderPath()
			if path == nil then
				break
			end

			local inst: Instance? = managedIndex[path]
			if inst ~= nil and inst:IsA("LuaSourceContainer") then
				local builderStart: number = os.clock()
				local result: string = Runtime.executeBuilder(path, inst :: LuaSourceContainer)
				local builderElapsed: number = os.clock() - builderStart
				recordBuilderPerf(path, result, builderElapsed * 1000)
				if BUILDERS.initialPending > 0 then
					if result == "executed" then
						BUILDERS.initialExecuted += 1
					elseif result == "skipped" then
						BUILDERS.initialSkipped += 1
					end
					BUILDERS.initialPending -= 1
					if BUILDERS.initialPending == 0 then
						BUILDERS.lastInitialCompletedFingerprint = BUILDERS.lastInitialQueuedFingerprint
						local totalElapsed: number = os.clock() - BUILDERS.initialStartedAt
						info(string.format("Initial builders complete: %d executed, %d skipped in %.1fms", BUILDERS.initialExecuted, BUILDERS.initialSkipped, totalElapsed * 1000))
						Workspace:SetAttribute("VertigoSyncWorldReady", true)
					end
				end
				if result ~= "failed" then
					info(string.format("  Builder %s took %.1fms", path, builderElapsed * 1000))
				end
			end

			processed += 1
		end

		BUILDERS.pumpActive = false
		updateBuilderPerfAttributes()
		if Runtime.hasQueuedBuilders() then
			Runtime.scheduleBuilderPump()
		end
	end)
end

BUILDERS.scheduleBatch = function()
	if BUILDERS.debounceScheduled then
		return
	end
	BUILDERS.debounceScheduled = true
	task.defer(function()
		task.wait(BUILD.DEBOUNCE_SECONDS)
		BUILDERS.debounceScheduled = false

		local dirtyPaths: { string } = {}
		for path: string in BUILDERS.dirtySet do
			table.insert(dirtyPaths, path)
		end
		BUILDERS.dirtySet = {}

		if #dirtyPaths == 0 then
			return
		end

		table.sort(dirtyPaths, function(a: string, b: string): boolean
			local aIsHub: boolean = string.find(a, "HubBuilder", 1, true) ~= nil
			local bIsHub: boolean = string.find(b, "HubBuilder", 1, true) ~= nil
			if aIsHub ~= bIsHub then
				return aIsHub
			end
			return a < b
		end)

		for _, path: string in dirtyPaths do
			Runtime.enqueueBuilderPath(path)
		end
		Runtime.scheduleBuilderPump()
	end)
end

BUILDERS.scheduleFullReconcile = function()
	if not BUILDERS.enabled then
		return
	end
	local builderPathCount: number = #PROJECT.builderRoots
	if builderPathCount == 0 then
		return
	end
	for path: string, inst: Instance in pairs(managedIndex) do
		if inst:IsA("LuaSourceContainer") then
			for i = 1, builderPathCount do
				local builderPrefix: string = PROJECT.builderRoots[i]
				if string.sub(path, 1, #builderPrefix) == builderPrefix then
					BUILDERS.dirtySet[path] = true
					BUILDERS.forceSet[path] = true
					break
				end
			end
		end
	end
	BUILDERS.scheduleBatch()
end

-- ─── DataModel mutation ─────────────────────────────────────────────────────

@native
function Runtime.applyWrite(path: string, source: string, sha256: string?)
	local parent, instanceName, className, boundary = Runtime.resolveTarget(path)
	if parent == nil or instanceName == nil or className == nil then
		throttledLog("resolve_" .. path, string.format("Cannot resolve target for write: %s", path), true)
		droppedUpdates += 1
		return
	end

	if SCRIPT_CLASSES[className] and #source >= CORE.MAX_LUA_SOURCE_LENGTH then
		Runtime.applyDelete(path)
		error(
			string.format(
				"OVERSIZE_SOURCE:%s exceeds Roblox ModuleScript/Script Source limit (%d >= %d)",
				path,
				#source,
				CORE.MAX_LUA_SOURCE_LENGTH
			)
		)
	end

	-- Handle StringValue type (e.g. .txt files)
	if className == "StringValue" then
		local inst: Instance = Runtime.ensureOrCreate(parent, instanceName, "StringValue")
		local stringInst: StringValue = inst :: StringValue
		if stringInst.Value ~= source then
			refreshSelfMutationGuard()
			stringInst.Value = source
		end
		inst:SetAttribute(CORE.MANAGED_PATH_ATTR, path)
		if sha256 ~= nil and sha256 ~= "" then
			inst:SetAttribute(CORE.MANAGED_SHA_ATTR, sha256)
			managedShaByPath[path] = sha256
		end
		managedIndex[path] = inst
		-- Apply meta if present
		local meta: EntryMeta? = metaByPath[path]
		if meta ~= nil then
			Runtime.applyMeta(inst, meta)
			metaByPath[path] = nil
		end
		return
	end

	-- Handle LocalizationTable type (e.g. .csv files)
	if className == "LocalizationTable" then
		local inst: Instance = Runtime.ensureOrCreate(parent, instanceName, "LocalizationTable")
		-- Try SetContents first for proper CSV parsing, fall back to attribute
		local csvOk = pcall(function()
			(inst :: any):SetContents(source)
		end)
		if not csvOk then
			inst:SetAttribute("CSVSource", source)
		end
		inst:SetAttribute(CORE.MANAGED_PATH_ATTR, path)
		if sha256 ~= nil and sha256 ~= "" then
			inst:SetAttribute(CORE.MANAGED_SHA_ATTR, sha256)
			managedShaByPath[path] = sha256
		end
		managedIndex[path] = inst
		local meta: EntryMeta? = metaByPath[path]
		if meta ~= nil then
			Runtime.applyMeta(inst, meta)
			metaByPath[path] = nil
		end
		return
	end

	local existingAny = parent:FindFirstChild(instanceName)
	local scriptInstance: LuaSourceContainer

	if existingAny ~= nil and existingAny:IsA("LuaSourceContainer") then
		if existingAny.ClassName ~= className then
			local replaced = Runtime.replaceInstanceClassPreservingChildren(existingAny, className)
			scriptInstance = replaced :: LuaSourceContainer
		elseif existingAny:IsA("ModuleScript") and (existingAny :: LuaSourceContainer).Source ~= source then
			-- Replace ModuleScripts on source change to invalidate Luau's require cache.
			-- Updating Source in-place keeps the cached module result alive, which means
			-- builder dependency edits do not actually take effect until Studio restarts.
			local replaced = Runtime.replaceInstanceClassPreservingChildren(existingAny, className)
			scriptInstance = replaced :: LuaSourceContainer
		else
			scriptInstance = existingAny
		end
	elseif existingAny ~= nil then
		local replaced = Runtime.replaceInstanceClassPreservingChildren(existingAny, className)
		scriptInstance = replaced :: LuaSourceContainer
	else
		refreshSelfMutationGuard()
		local created = Runtime.poolGet(className)
		created.Name = instanceName
		created.Parent = parent
		scriptInstance = created :: LuaSourceContainer
	end

	if scriptInstance.Source ~= source then
		refreshSelfMutationGuard()
		scriptInstance.Source = source
	end

	local runContext: Enum.RunContext? = Runtime.runContextForPath(path)
	if runContext ~= nil and scriptInstance:IsA("Script") then
		pcall(function()
			(scriptInstance :: Script).RunContext = runContext
		end)
	end

	scriptInstance:SetAttribute(CORE.MANAGED_PATH_ATTR, path)
	if sha256 ~= nil and sha256 ~= "" then
		scriptInstance:SetAttribute(CORE.MANAGED_SHA_ATTR, sha256)
		managedShaByPath[path] = sha256
	end

	managedIndex[path] = scriptInstance

	-- Apply .meta.json properties if present
	local meta: EntryMeta? = metaByPath[path]
	if meta ~= nil then
		Runtime.applyMeta(scriptInstance, meta)
		metaByPath[path] = nil
	end

	if boundary ~= nil and scriptInstance.Parent == nil then
		warnMsg(string.format("Write detached unexpectedly for %s under %s", path, boundary:GetFullName()))
	end

	-- Builder re-execution check — direct builder change or dependency cascade
	if BUILDERS.enabled then
		local isBuilder: boolean = false
		local builderPathCount: number = #PROJECT.builderRoots
		for i = 1, builderPathCount do
			if string.sub(path, 1, #PROJECT.builderRoots[i]) == PROJECT.builderRoots[i] then
				isBuilder = true
				break
			end
		end

		if isBuilder then
			-- Direct builder change — add to dirty set and schedule batch
			BUILDERS.dirtySet[path] = true
			BUILDERS.scheduleBatch()
		else
			-- Check if this is a shared dependency that builders depend on
			local depPathCount: number = #PROJECT.builderDependencyRoots
			for i = 1, depPathCount do
				local depPrefix: string = PROJECT.builderDependencyRoots[i]
				if string.sub(path, 1, #depPrefix) == depPrefix then
					-- Cascade: find all builders that depend on this prefix
					local dependentBuilders: { [string]: boolean }? = BUILDERS.dependencyMap[depPrefix]
					if dependentBuilders ~= nil then
						for builderPath: string in dependentBuilders do
							BUILDERS.dirtySet[builderPath] = true
						end
						BUILDERS.scheduleBatch()
					end
					break
				end
			end
		end
	end
end

@native
function Runtime.applyDelete(path: string)
	local _parent, _instanceName, _className, boundary = Runtime.resolveTarget(path)
	local existing = managedIndex[path]
	if existing ~= nil and existing.Parent ~= nil then
		local parent = existing.Parent
		refreshSelfMutationGuard()
		Runtime.poolReturn(existing)
		managedIndex[path] = nil
		managedShaByPath[path] = nil
		metaByPath[path] = nil
		Runtime.cleanupEmptyAncestors(parent, boundary)
		return
	end

	local parent, instanceName = _parent, _instanceName
	if parent == nil or instanceName == nil then
		return
	end
	local target = parent:FindFirstChild(instanceName)
	if target ~= nil then
		local targetParent = target.Parent
		refreshSelfMutationGuard()
		Runtime.poolReturn(target)
		Runtime.cleanupEmptyAncestors(targetParent, boundary)
	end
	managedIndex[path] = nil
	managedShaByPath[path] = nil
	hardRejectedShaByPath[path] = nil
	hardRejectedReasonByPath[path] = nil
	metaByPath[path] = nil
end

-- ─── Pending operation coalescing ───────────────────────────────────────────

@native
function Runtime.stageOperation(path: string, action: PendingAction, expectedSha: string?)
	local mapping, _remainder = resolveMapping(path)
	if mapping == nil then
		throttledLog("unmappable_" .. path, "Dropped unmappable path: " .. path, true)
		droppedUpdates += 1
		return
	end

	-- Skip binary model entries if feature is disabled
	local fileType: string? = Runtime.fileTypeForPath(path)
	if Runtime.isBinaryModelType(fileType) and not SETTINGS.binaryModels then
		return
	end

	-- Binary model delete: clean up model instances
	if Runtime.isBinaryModelType(fileType) and action == "delete" then
		Runtime.cleanupModelInstances(path)
		-- Still process through normal delete path for managed index cleanup
	end

	opEpoch += 1
	local currentEpoch: number = opEpoch
	local op: PendingOp? = pendingOps[path]
	if op == nil then
		pendingOps[path] = {
			action = action,
			epoch = currentEpoch,
			retries = 0,
			queued = false,
			expectedSha = expectedSha,
		}
	else
		op.action = action
		op.epoch = currentEpoch
		op.retries = 0
		op.expectedSha = expectedSha
	end

	if action == "delete" then
		readySources[path] = nil
		readyModels[path] = nil
	end

	Runtime.enqueuePath(path)

	if action == "write" then
		-- Binary models: spawn a dedicated manifest fetch instead of using the source fetch queue
		if Runtime.isBinaryModelType(fileType) then
			local capturedEpoch: number = currentEpoch
			local capturedSha: string? = expectedSha
			task.spawn(function()
				-- Verify the op is still current before fetching
				local currentOp: PendingOp? = pendingOps[path]
				if currentOp == nil or currentOp.epoch ~= capturedEpoch then
					return
				end

				local ok, manifest, _sha, statusCode, err = requestModelManifest(path)
				if not ok or manifest == nil then
					if statusCode == 404 then
						Runtime.stageDelete(path)
						return
					end
					droppedUpdates += 1
					throttledLog("model_fetch_fail_" .. path, string.format("Model manifest fetch failed for %s: %s", path, tostring(err)), true)
					return
				end

				-- Verify the op is still current after async fetch
				currentOp = pendingOps[path]
				if currentOp == nil or currentOp.epoch ~= capturedEpoch then
					return
				end

				Runtime.stageModelManifest(path, manifest, capturedEpoch, capturedSha)
			end)
		else
			Runtime.pushFetchTask(path, currentEpoch)
		end
	end
end

@native
function Runtime.stageWrite(path: string, expectedSha: string?)
	Runtime.stageOperation(path, "write", expectedSha)
end

@native
function Runtime.isLiveSyncWritableSourceEntry(path: string, bytes: number): (boolean, string?)
	if bytes < CORE.MAX_LUA_SOURCE_LENGTH then
		return true, nil
	end

	local mapping, remainder = resolveMapping(path)
	if mapping == nil or remainder == nil then
		return true, nil
	end

	local _segments, className, _instanceName = Runtime.parseRelativePath(remainder)
	if SCRIPT_CLASSES[className] then
		return false, string.format(
			"LIVE_SYNC_SKIPPED_OVERSIZE:%s exceeds Roblox ModuleScript/Script Source limit (%d >= %d)",
			path,
			bytes,
			CORE.MAX_LUA_SOURCE_LENGTH
		)
	end

	return true, nil
end

@native
function Runtime.beginLiveSyncSkipAggregation(context: string)
	SERVER.liveSyncSkipAggregation = {
		context = context,
		count = 0,
		samplePaths = table.create(6),
	}
end

@native
function Runtime.recordLiveSyncSkipAggregation(path: string, reason: string)
	local aggregation = SERVER.liveSyncSkipAggregation
	if aggregation == nil then
		warnMsg(string.format("Live sync skipped for %s: %s", path, reason))
		return
	end

	aggregation.count += 1
	if #aggregation.samplePaths < 6 then
		table.insert(aggregation.samplePaths, path)
	end
end

@native
function Runtime.flushLiveSyncSkipAggregation()
	local aggregation = SERVER.liveSyncSkipAggregation
	SERVER.liveSyncSkipAggregation = nil
	if aggregation == nil or aggregation.count == 0 then
		return
	end

	local sampleCount = #aggregation.samplePaths
	local sampleText = ""
	if sampleCount > 0 then
		sampleText = string.format("; samples: %s", table.concat(aggregation.samplePaths, ", "))
	end

	local omittedCount = aggregation.count - sampleCount
	local remainderText = ""
	if omittedCount > 0 then
		remainderText = string.format(" (+%d more)", omittedCount)
	end

	warnMsg(string.format(
		"LIVE_SYNC_SKIPPED_OVERSIZE_SUMMARY:%s skipped %d oversized live-sync source entries (limit=%d)%s%s",
		aggregation.context,
		aggregation.count,
		CORE.MAX_LUA_SOURCE_LENGTH,
		sampleText,
		remainderText
	))
end

@native
function Runtime.stageWriteEntry(path: string, expectedSha: string?, bytes: number?, meta: EntryMeta?)
	if meta ~= nil then
		metaByPath[path] = meta
	end

	if type(bytes) == "number" then
		local writable, reason = Runtime.isLiveSyncWritableSourceEntry(path, bytes)
		if not writable and reason ~= nil then
			local rejectedSha: string = expectedSha or ""
			if hardRejectedShaByPath[path] ~= rejectedSha or hardRejectedReasonByPath[path] ~= reason then
				hardRejectedShaByPath[path] = rejectedSha
				hardRejectedReasonByPath[path] = reason
				Runtime.recordLiveSyncSkipAggregation(path, reason)
			end
			return
		end
	end

	local rejectedSha: string? = hardRejectedShaByPath[path]
	if rejectedSha ~= nil and expectedSha ~= nil and rejectedSha ~= expectedSha then
		hardRejectedShaByPath[path] = nil
		hardRejectedReasonByPath[path] = nil
	end

	Runtime.stageWrite(path, expectedSha)
end

@native
function Runtime.stageDelete(path: string)
	Runtime.stageOperation(path, "delete", nil)
end

--- Rename an existing managed instance instead of deleting + recreating.
--- Preserves instance references, runtime state, and selection state.
--- Falls back to delete + write if the source instance is missing.
function Runtime.stageRename(oldPath: string, newPath: string, sha256: string?)
	local existing: Instance? = managedIndex[oldPath]
	if existing == nil or (existing :: Instance).Parent == nil then
		-- Fallback: treat as delete + add
		Runtime.stageDelete(oldPath)
		Runtime.stageWrite(newPath, sha256)
		return
	end

	-- Resolve new target location
	local newParent, newInstanceName, newClassName, newBoundary = Runtime.resolveTarget(newPath)
	if newParent == nil or newInstanceName == nil then
		Runtime.stageDelete(oldPath)
		Runtime.stageWrite(newPath, sha256)
		return
	end

	-- Rename in place: update Name and reparent if needed
	refreshSelfMutationGuard()
	local inst: Instance = existing :: Instance
	if inst.Name ~= newInstanceName then
		inst.Name = newInstanceName :: string
	end
	if inst.Parent ~= newParent then
		inst.Parent = newParent
	end

	-- Update tracking indices
	inst:SetAttribute(CORE.MANAGED_PATH_ATTR, newPath)
	managedIndex[newPath] = inst
	managedIndex[oldPath] = nil
	if sha256 then
		inst:SetAttribute(CORE.MANAGED_SHA_ATTR, sha256)
		managedShaByPath[newPath] = sha256
	end
	managedShaByPath[oldPath] = nil

	-- Migrate meta cache
	if metaByPath[oldPath] ~= nil then
		metaByPath[newPath] = metaByPath[oldPath]
		metaByPath[oldPath] = nil
	end

	-- Clean up empty ancestor folders from old location
	local _oldParent, _oldName, _oldClass, oldBoundary = Runtime.resolveTarget(oldPath)
	if _oldParent and _oldParent ~= inst.Parent then
		Runtime.cleanupEmptyAncestors(_oldParent, oldBoundary)
	end
end

@native
function Runtime.stagePaths(paths: any, action: PendingAction)
	if type(paths) ~= "table" then
		return
	end
	local pathCount: number = #paths
	for i = 1, pathCount do
		local rawPath: any = paths[i]
		if type(rawPath) == "string" and rawPath ~= "" then
			if action == "write" then
				Runtime.stageWrite(rawPath, nil)
			else
				Runtime.stageDelete(rawPath)
			end
		end
	end
end

-- ─── Snapshot + diff reconciliation ─────────────────────────────────────────

@native
function Runtime.reconcileSnapshot(snapshot: SnapshotResponse)
	local entries: { SnapshotEntry } = snapshot.entries
	local entryCount: number = #entries
	local seenPaths: { [string]: boolean } = {}

	for i = 1, entryCount do
		local entry: SnapshotEntry = entries[i]
		seenPaths[entry.path] = true
	end

	for path: string, _sha: string in pairs(managedShaByPath) do
		if not seenPaths[path] then
			Runtime.stageDelete(path)
		end
	end

	for i = 1, entryCount do
		local entry: SnapshotEntry = entries[i]
		local entryPath: string = entry.path
		local entrySha: string = entry.sha256

		local rejectedSha: string? = hardRejectedShaByPath[entryPath]
		if rejectedSha ~= nil and rejectedSha == entrySha then
			continue
		end
		if rejectedSha ~= nil and rejectedSha ~= entrySha then
			hardRejectedShaByPath[entryPath] = nil
			hardRejectedReasonByPath[entryPath] = nil
		end

		if managedShaByPath[entryPath] ~= entrySha then
			if SERVER.liveSyncSkipAggregation == nil then
				Runtime.beginLiveSyncSkipAggregation("snapshot_reconcile")
			end
			Runtime.stageWriteEntry(entryPath, entrySha, entry.bytes, entry.meta)
		end
	end
	Runtime.flushLiveSyncSkipAggregation()

	lastHash = snapshot.fingerprint
	setStatusAttributes("connected", snapshot.fingerprint)
end

@native
function Runtime.beginFullResync()
	opEpoch += 1
	pendingOps = {}
	pendingQueue = {}
	pendingQueueHead = 1
	fetchQueue = {}
	fetchQueueHead = 1
	fetchQueuedEpoch = {}
	inflightFetchEpoch = {}
	readySources = {}
	readyModels = {}
	modelApplyQueues = {}
	modelApplyQueueHeads = {}
	modelBuildLookup = {}
	modelApplyActive = {}
end

function Runtime.syncFromSnapshot(reason: string): boolean
	local ok, payloadOrErr = requestJson("/snapshot")
	if not ok then
		warnMsg(string.format("Snapshot sync failed (%s): %s", reason, tostring(payloadOrErr)))
		setStatusAttributes("error", lastHash)
		return false
	end

	local payload = payloadOrErr :: any
	if type(payload) ~= "table" then
		warnMsg(string.format("Snapshot sync failed (%s): malformed payload", reason))
		setStatusAttributes("error", lastHash)
		return false
	end

	if type(payload.fingerprint) ~= "string" or type(payload.entries) ~= "table" then
		warnMsg(string.format("Snapshot sync failed (%s): missing fingerprint/entries", reason))
		setStatusAttributes("error", lastHash)
		return false
	end

	local snapshot = payload :: SnapshotResponse
	Runtime.beginFullResync()
	bootstrapManagedIndex()
	Runtime.reconcileSnapshot(snapshot)
	resyncRequested = false
	consecutiveErrors = 0
	pollInterval = CORE.POLL_INTERVAL_FAST
	info(string.format("Snapshot reconciled (%s). fingerprint=%s entries=%d", reason, snapshot.fingerprint, #snapshot.entries))
	if BUILDERS.enabled and isEditMode() then
		task.defer(runInitialBuilders)
	end
	return true
end

@native
function Runtime.pollDiff()
	if lastHash == nil then
		return
	end

	local previousHash = lastHash
	local endpoint = string.format("/diff?since=%s", HttpService:UrlEncode(previousHash))
	local ok, payloadOrErr, statusCode = requestJson(endpoint)
	if not ok then
		if statusCode == 404 then
			resyncRequested = true
			nextPollAt = 0
			throttledLog("diff_history_miss", string.format("Diff history miss for %s; requesting snapshot resync", previousHash), true)
			return
		end
		consecutiveErrors += 1
		pollInterval = math.min(CORE.POLL_INTERVAL_MAX, pollInterval * 1.6)
		throttledLog("diff_poll_fail", string.format("Diff poll failed (attempt=%d): %s", consecutiveErrors, tostring(payloadOrErr)), true)
		if consecutiveErrors >= 5 then
			setStatusAttributes("error", lastHash)
		end
		return
	end

	consecutiveErrors = 0
	pollInterval = CORE.POLL_INTERVAL_FAST

	local payload = payloadOrErr :: any
	if type(payload) ~= "table" then
		warnMsg("Diff payload malformed: expected table")
		return
	end

	if type(payload.current_fingerprint) ~= "string" then
		warnMsg("Diff payload missing current_fingerprint")
		return
	end

	local diff = payload :: DiffResponse
	if type(diff.previous_fingerprint) ~= "string" then
		warnMsg("Diff payload missing previous_fingerprint")
		resyncRequested = true
		nextPollAt = 0
		return
	end

	if diff.previous_fingerprint ~= previousHash then
		resyncRequested = true
		nextPollAt = 0
		warnMsg(
			string.format(
				"Diff base fingerprint mismatch (expected=%s actual=%s); requesting snapshot resync",
				previousHash,
				diff.previous_fingerprint
			)
		)
		return
	end

	if diff.current_fingerprint == previousHash then
		return
	end

	if type(diff.deleted) == "table" then
		for _, entry in ipairs(diff.deleted) do
			if type(entry) == "table" and type(entry.path) == "string" then
				Runtime.stageDelete(entry.path)
			end
		end
	end
	-- Process renames: move instance in-place instead of delete+recreate
	if type(diff.renamed) == "table" then
		for _, entry in ipairs(diff.renamed) do
			if type(entry) == "table" and type(entry.old_path) == "string" and type(entry.new_path) == "string" then
				Runtime.stageRename(entry.old_path, entry.new_path, entry.sha256)
			end
		end
	end
	if type(diff.added) == "table" then
		Runtime.beginLiveSyncSkipAggregation("diff_added")
		for _, entry in ipairs(diff.added) do
			if type(entry) == "table" and type(entry.path) == "string" then
				Runtime.stageWriteEntry(entry.path, entry.sha256, entry.bytes, entry.meta)
			end
		end
		Runtime.flushLiveSyncSkipAggregation()
	end
	if type(diff.modified) == "table" then
		Runtime.beginLiveSyncSkipAggregation("diff_modified")
		for _, entry in ipairs(diff.modified) do
			if type(entry) == "table" and type(entry.path) == "string" then
				Runtime.stageWriteEntry(entry.path, entry.current_sha256, entry.current_bytes, entry.meta)
			end
		end
		Runtime.flushLiveSyncSkipAggregation()
	end

	lastHash = diff.current_fingerprint
	setStatusAttributes("connected", diff.current_fingerprint)

	local diffFileCount: number = (if type(diff.added) == "table" then #diff.added else 0)
		+ (if type(diff.modified) == "table" then #diff.modified else 0)
		+ (if type(diff.deleted) == "table" then #diff.deleted else 0)
		+ (if type(diff.renamed) == "table" then #diff.renamed else 0)
end

-- ─── Fetch workers ──────────────────────────────────────────────────────────

@native
function Runtime.processFetchQueue()
	local function scheduleFetchRetry(path: string, epoch: number, reason: string?)
		local current = pendingOps[path]
		if current == nil or current.action ~= "write" or current.epoch ~= epoch then
			return
		end

		current.retries += 1
		if current.retries > CORE.MAX_SOURCE_FETCH_RETRIES then
			droppedUpdates += 1
			throttledLog("fetch_exhaust_" .. path, string.format("Source fetch retries exhausted for %s (%s)", path, tostring(reason)), true)
			resyncRequested = true
			return
		end

		local backoff = math.min(0.15 * current.retries, 0.75)
		task.delay(backoff, function()
			local stillCurrent = pendingOps[path]
			if stillCurrent and stillCurrent.action == "write" and stillCurrent.epoch == epoch then
				Runtime.pushFetchTask(path, epoch)
			end
		end)
	end

	while fetchInFlight < adaptiveFetchConcurrency do
		local availableSlots = math.max(adaptiveFetchConcurrency - fetchInFlight, 0)
		if availableSlots <= 0 then
			return
		end

		local batchCap = clampNumber(availableSlots, 1, CORE.MAX_SOURCE_BATCH_SIZE)
		local batchPaths: { string } = table.create(batchCap)
		local batchEpochByPath: { [string]: number } = {}

		while #batchPaths < batchCap do
			local taskItem = Runtime.popFetchTask()
			if taskItem == nil then
				break
			end

			local path = taskItem.path
			local epoch = taskItem.epoch
			fetchQueuedEpoch[path] = nil

			local op = pendingOps[path]
			if op == nil or op.action ~= "write" or op.epoch ~= epoch then
				continue
			end

			if batchEpochByPath[path] == nil then
				if #batchPaths > 0 and wouldExceedSourceBatchEndpointLimit(batchPaths, path) then
					Runtime.pushFetchTask(path, epoch)
					break
				end
				table.insert(batchPaths, path)
			end
			batchEpochByPath[path] = epoch
		end

		local batchSize = #batchPaths
		if batchSize == 0 then
			return
		end

		fetchInFlight += batchSize
		for i = 1, batchSize do
			local path = batchPaths[i]
			local epoch = batchEpochByPath[path]
			inflightFetchEpoch[path] = epoch
		end

		task.spawn(function()
			local function completeInflight()
				fetchInFlight -= batchSize
				if fetchInFlight < 0 then
					fetchInFlight = 0
				end
				for i = 1, batchSize do
					local path = batchPaths[i]
					local epoch = batchEpochByPath[path]
					if inflightFetchEpoch[path] == epoch then
						inflightFetchEpoch[path] = nil
					end
				end
			end

			if batchSize == 1 then
				local path = batchPaths[1]
				local epoch = batchEpochByPath[path]
				local ok, source, sha256, statusCode, err = requestSource(path)
				completeInflight()

				local current = pendingOps[path]
				if current == nil or current.action ~= "write" or current.epoch ~= epoch then
					return
				end

				if ok and source ~= nil then
					readySources[path] = {
						epoch = epoch,
						source = source,
						sha256 = sha256,
					}
					Runtime.enqueuePath(path)
					return
				end

				if statusCode == 404 then
					Runtime.stageDelete(path)
					return
				end

				scheduleFetchRetry(path, epoch, err)
				return
			end

			local ok, payload, statusCode, err = requestSourcesBatch(batchPaths)
			completeInflight()

			if not ok or payload == nil then
				if statusCode == 413 then
					-- Payload too large: force single-file fetches and requeue without spending retries.
					adaptiveFetchConcurrency = FETCH.CONCURRENCY_MIN
					for i = 1, batchSize do
						local path = batchPaths[i]
						local epoch = batchEpochByPath[path]
						local current = pendingOps[path]
						if current ~= nil and current.action == "write" and current.epoch == epoch then
							Runtime.pushFetchTask(path, epoch)
						end
					end
					task.defer(Runtime.processFetchQueue)
					return
				end
				for i = 1, batchSize do
					local path = batchPaths[i]
					local epoch = batchEpochByPath[path]
					scheduleFetchRetry(path, epoch, err)
				end
				return
			end

			local foundByPath: { [string]: SourceContentEntry } = {}
			local entries = payload.entries
			for i = 1, #entries do
				local entry = entries[i]
				if type(entry) == "table" and type(entry.path) == "string" and type(entry.content) == "string" then
					foundByPath[entry.path] = entry
				end
			end

			local missingByPath: { [string]: boolean } = {}
			local missing = payload.missing
			if type(missing) == "table" then
				for i = 1, #missing do
					local missingPath = missing[i]
					if type(missingPath) == "string" then
						missingByPath[missingPath] = true
					end
				end
			end

			for i = 1, batchSize do
				local path = batchPaths[i]
				local epoch = batchEpochByPath[path]
				local current = pendingOps[path]
				if current == nil or current.action ~= "write" or current.epoch ~= epoch then
					continue
				end

				local entry = foundByPath[path]
				if entry ~= nil then
					readySources[path] = {
						epoch = epoch,
						source = entry.content,
						sha256 = entry.sha256,
						meta = entry.meta,
					}
					Runtime.enqueuePath(path)
				elseif missingByPath[path] then
					Runtime.stageDelete(path)
				else
					scheduleFetchRetry(path, epoch, err or "batch response missing path")
				end
			end
		end)
	end
end

-- ─── Apply loop ─────────────────────────────────────────────────────────────

@native
function Runtime.recalcAdaptiveThresholds(appliedThisTick: number, tickElapsedSeconds: number)
	if appliedThisTick > 0 and tickElapsedSeconds > 0 then
		local perApplySeconds = tickElapsedSeconds / appliedThisTick
		if applyCostEwmaSeconds <= 0 then
			applyCostEwmaSeconds = perApplySeconds
		else
			local alpha = APPLY.BUDGET_EWMA_ALPHA
			applyCostEwmaSeconds = applyCostEwmaSeconds * (1 - alpha) + perApplySeconds * alpha
		end
	elseif applyCostEwmaSeconds <= 0 then
		applyCostEwmaSeconds = APPLY.FRAME_BUDGET_SECONDS / APPLY.MAX_PER_TICK
	end

	local now = os.clock()
	if now - lastAdaptiveRecalcAt < APPLY.BUDGET_RECALC_SECONDS then
		return
	end
	lastAdaptiveRecalcAt = now

	local pendingDepth = math.max(#pendingQueue - pendingQueueHead + 1, 0)
	local fetchDepth = math.max(#fetchQueue - fetchQueueHead + 1, 0)
	local backlogRatio = clampNumber(pendingDepth / APPLY.QUEUE_HIGH_WATERMARK, 0, 4)

	local targetBudget = APPLY.FRAME_BUDGET_SECONDS * (1 + 0.5 * backlogRatio)
	adaptiveApplyBudgetSeconds = clampNumber(targetBudget, APPLY.FRAME_BUDGET_MIN_SECONDS, APPLY.FRAME_BUDGET_MAX_SECONDS)

	local opCost = if applyCostEwmaSeconds > 0
		then applyCostEwmaSeconds
		else (APPLY.FRAME_BUDGET_SECONDS / APPLY.MAX_PER_TICK)

	local budgetedOps = math.floor((adaptiveApplyBudgetSeconds / opCost) * 0.9 + 0.5)
	local backlogBoost = math.floor(pendingDepth / 96)
	local targetMaxApplies = budgetedOps + backlogBoost
	targetMaxApplies = clampNumber(targetMaxApplies, APPLY.MIN_PER_TICK, APPLY.MAX_HARD_LIMIT)
	adaptiveMaxAppliesPerTick = math.floor(targetMaxApplies + 0.5)

	local fetchBoost = math.floor(fetchDepth / 64)
	local targetFetchConcurrency = CORE.MAX_FETCH_CONCURRENCY + fetchBoost
	targetFetchConcurrency = clampNumber(targetFetchConcurrency, FETCH.CONCURRENCY_MIN, FETCH.CONCURRENCY_MAX)
	adaptiveFetchConcurrency = math.floor(targetFetchConcurrency + 0.5)
end

@native
function Runtime.processApplyQueue()
	if not syncEnabled and not HISTORY.active then
		return
	end
	if not isEditMode() then
		return
	end

	local tickStart: number = os.clock()
	local appliedThisTick: number = 0
	local budgetSeconds: number = adaptiveApplyBudgetSeconds
	local maxApplies: number = adaptiveMaxAppliesPerTick

	while appliedThisTick < maxApplies do
		if os.clock() - tickStart >= budgetSeconds then
			break
		end

		local path: string? = Runtime.popPendingPath()
		if path == nil then
			break
		end

		local op: PendingOp? = pendingOps[path]
		if op == nil then
			continue
		end

		op.queued = false

		if op.action == "delete" then
			local ok: boolean, err: any = pcall(applyDelete, path)
			if not ok then
				droppedUpdates += 1
				warnMsg(string.format("Delete apply failed for %s: %s", path, tostring(err)))
			end
			pendingOps[path] = nil
			readySources[path] = nil
			appliedThisTick += 1
			continue
		end

		-- Binary model apply: process one instance from the staged queue per tick iteration
		if op.action == "model_apply" then
			local created: boolean = pcall(processOneModelOp, path)
			appliedThisTick += 1

			-- Check if there are more ops remaining
			local remainingOps: { ModelApplyOp }? = modelApplyQueues[path]
			local remainingHead: number = modelApplyQueueHeads[path] or 1
			if remainingOps == nil or remainingHead > #remainingOps then
				-- Queue exhausted — model fully applied
				pendingOps[path] = nil
				modelApplyActive[path] = nil
				modelApplyQueues[path] = nil
				modelApplyQueueHeads[path] = nil
				info(string.format("Model fully applied: %s", path))
			else
				-- More ops remaining — re-enqueue for next tick iteration
				op.queued = false
				Runtime.enqueuePath(path)
			end
			continue
		end

		local ready: ReadySource? = readySources[path]
		if ready == nil or ready.epoch ~= op.epoch then
			if inflightFetchEpoch[path] ~= op.epoch then
				Runtime.pushFetchTask(path, op.epoch)
			end
			continue
		end

		-- Store meta from ready source for applyWrite to consume
		if ready.meta ~= nil then
			metaByPath[path] = ready.meta
		end

		local writeOk: boolean, writeErr: any = pcall(Runtime.applyWrite, path, ready.source, ready.sha256 or op.expectedSha)
		if writeOk then
			pendingOps[path] = nil
			readySources[path] = nil
			hardRejectedShaByPath[path] = nil
			hardRejectedReasonByPath[path] = nil
			appliedThisTick += 1
		else
			local writeErrText: string = tostring(writeErr)
			if string.sub(writeErrText, 1, 16) == "OVERSIZE_SOURCE:" then
				droppedUpdates += 1
				local rejectedSha: string = ready.sha256 or op.expectedSha or ""
				hardRejectedShaByPath[path] = rejectedSha
				hardRejectedReasonByPath[path] = writeErrText
				warnMsg(string.format("Write apply rejected for %s: %s", path, writeErrText))
				pendingOps[path] = nil
				readySources[path] = nil
				resyncRequested = true
				continue
			end
			op.retries += 1
			if op.retries > CORE.MAX_SOURCE_FETCH_RETRIES then
				droppedUpdates += 1
				warnMsg(string.format("Write apply permanently failed for %s after %d retries: %s", path, op.retries, writeErrText))
				pendingOps[path] = nil
				readySources[path] = nil
				resyncRequested = true
			else
				-- Retry: keep the ready source, re-enqueue for next tick
				warnMsg(string.format("Write apply failed for %s (retry %d): %s", path, op.retries, writeErrText))
				op.queued = false
				task.defer(function()
					local stillCurrent = pendingOps[path]
					if stillCurrent and stillCurrent.epoch == op.epoch then
						Runtime.enqueuePath(path)
					end
				end)
			end
		end
	end

	if appliedThisTick > 0 then
		appliedInWindow += appliedThisTick
		local now: number = os.clock()
		local elapsed: number = now - applyWindowStart
		if elapsed >= 1.0 then
			appliedPerSecond = math.floor(appliedInWindow / elapsed + 0.5)
			appliedInWindow = 0
			applyWindowStart = now
		end
	end

	local tickElapsed: number = os.clock() - tickStart
	Runtime.recalcAdaptiveThresholds(appliedThisTick, tickElapsed)
end

local function formatCompactMetricCount(value: number): string
	if value < 1000 then
		return tostring(value)
	end
	if value < 10000 then
		return string.format("%.1fk", value / 1000)
	end
	if value < 1000000 then
		return string.format("%dk", math.floor(value / 1000 + 0.5))
	end
	if value < 10000000 then
		return string.format("%.1fm", value / 1000000)
	end
	return string.format("%dm", math.floor(value / 1000000 + 0.5))
end

-- ─── Time-Travel Logic ──────────────────────────────────────────────────────
-- Consolidated into a single table to stay under Luau's 200 local register limit.
-- (do...end blocks do NOT create new register scopes in the bytecode compiler.)

local TimeTravel = {}

function TimeTravel.fetchHistory(force: boolean?): boolean
	if HISTORY.fetchInFlight then
		return HISTORY.loaded
	end

	local now: number = os.clock()
	if not force and HISTORY.loaded and (now - HISTORY.lastFetchAt) < FEATURES.HISTORY_REFRESH_INTERVAL_SECONDS then
		return true
	end

	HISTORY.fetchInFlight = true
	local endpoint: string = string.format("/history?limit=%d", SETTINGS.historyBuffer)
	local ok: boolean, payload: any = requestJson(endpoint)
	HISTORY.lastFetchAt = now
	HISTORY.fetchInFlight = false
	if not ok then
		warnMsg(string.format("History fetch failed: %s", tostring(payload)))
		HISTORY.fetchFailed = true
		return false
	end
	if type(payload) ~= "table" then
		warnMsg("History payload malformed")
		HISTORY.fetchFailed = true
		return false
	end
	-- Validate history entry shape
	local validated: { HistoryEntry } = {}
	for _, entry in ipairs(payload) do
			if
				type(entry) == "table"
				and type(entry.seq) == "number"
				and type(entry.fingerprint) == "string"
				and type(entry.timestamp) == "string"
				and type(entry.added) == "number"
				and type(entry.modified) == "number"
				and type(entry.deleted) == "number"
			then
				if type(entry.geometry_affecting) ~= "boolean" then
					entry.geometry_affecting = false
				end
				if type(entry.scope) ~= "string" then
					entry.scope = ""
				end
				table.insert(validated, entry :: HistoryEntry)
			end
		end
	HISTORY.entries = validated
	HISTORY.loaded = true
	HISTORY.fetchFailed = false
	return true
end

function TimeTravel.rewindToIndex(targetIndex: number)
	if targetIndex < 1 or targetIndex > #HISTORY.entries then
		return
	end
	if HISTORY.busy then
		HISTORY.pendingResumeLive = false
		HISTORY.pendingIndex = targetIndex
		return
	end
	if HISTORY.active and HISTORY.currentIndex == targetIndex then
		return
	end

	local targetFingerprint: string = HISTORY.entries[targetIndex].fingerprint
	local previewInvalidationNeeded = historyTransitionAffectsPreview(HISTORY.currentIndex, targetIndex)
	HISTORY.active = true
	HISTORY.busy = true

	-- Pause normal sync
	syncEnabled = false

	-- Request the exact historical snapshot from the server.
	local ok: boolean, payload: any = requestJson("/snapshot?at=" .. HttpService:UrlEncode(targetFingerprint))
	if not ok then
		warnMsg("Rewind failed: " .. tostring(payload))
		syncEnabled = true
		HISTORY.active = false
		HISTORY.busy = false
		return
	end

	if type(payload) ~= "table" then
		warnMsg("Rewind payload malformed")
		syncEnabled = true
		HISTORY.active = false
		HISTORY.busy = false
		return
	end

	-- Apply the exact historical snapshot through the normal pipeline.
	local snapshot = payload :: SnapshotResponse
	Runtime.beginFullResync()
	bootstrapManagedIndex()
	Runtime.reconcileSnapshot(snapshot)

	lastHash = snapshot.fingerprint
	HISTORY.currentIndex = targetIndex
	HISTORY.needsBuilderReconcile = BUILDERS.enabled
	bumpTimeTravelEpoch()
	if previewInvalidationNeeded then
		bumpPreviewInvalidationEpoch()
	end

	setStatusAttributes("connected", snapshot.fingerprint)
	publishTimeTravelAttributes()

	if HISTORY.needsBuilderReconcile and BUILDERS.enabled and isEditMode() and BUILDERS.scheduleFullReconcile ~= nil then
		local drained: boolean = false
		local drainDeadline: number = os.clock() + 5
		while os.clock() < drainDeadline do
			local pendingDepth: number = math.max(#pendingQueue - pendingQueueHead + 1, 0)
			local fetchDepth: number = math.max(#fetchQueue - fetchQueueHead + 1, 0)
			if pendingDepth == 0 and fetchDepth == 0 and fetchInFlight == 0 then
				drained = true
				break
			end
			task.wait()
		end
		HISTORY.needsBuilderReconcile = false
		BUILDERS.scheduleFullReconcile()
		if not drained then
			warnMsg("Historical snapshot writes exceeded drain timeout; builder reconcile forced anyway")
		end
	end

	local scopeLabel = if HISTORY.entries[targetIndex].geometry_affecting then "geometry" else (HISTORY.entries[targetIndex].scope or "code")
	info(string.format("Rewound to seq %d [%s] (fingerprint=%s)", HISTORY.entries[targetIndex].seq, scopeLabel, targetFingerprint))
	HISTORY.busy = false
	local pendingResumeLive = HISTORY.pendingResumeLive
	local pendingIndex = HISTORY.pendingIndex
	HISTORY.pendingResumeLive = false
	HISTORY.pendingIndex = nil
	if pendingResumeLive then
		task.defer(TimeTravel.resumeLiveSync)
	elseif pendingIndex and pendingIndex ~= HISTORY.currentIndex then
		task.defer(TimeTravel.rewindToIndex, pendingIndex)
	end
end

function TimeTravel.resumeLiveSync()
	if HISTORY.busy then
		HISTORY.pendingIndex = nil
		HISTORY.pendingResumeLive = true
		return
	end
	HISTORY.active = false
	HISTORY.currentIndex = 0
	HISTORY.pendingIndex = nil
	HISTORY.pendingResumeLive = false
	HISTORY.needsBuilderReconcile = BUILDERS.enabled
	local previewInvalidationNeeded = historyTransitionAffectsPreview(HISTORY.currentIndex, 0)
	bumpTimeTravelEpoch()
	if previewInvalidationNeeded then
		bumpPreviewInvalidationEpoch()
	end
	syncEnabled = true
	resyncRequested = true -- Force full resync to get back to current state
	publishTimeTravelAttributes()
	if currentStatus == "connected" then
		TimeTravel.fetchHistory(true)
	end
	info("Resumed live sync")
end

function TimeTravel.stepBackward()
	if not HISTORY.loaded or #HISTORY.entries == 0 then
		return
	end
	local targetIndex: number
	if HISTORY.currentIndex == 0 then
		-- First step back from live: go to latest history entry
		targetIndex = #HISTORY.entries
	else
		targetIndex = HISTORY.currentIndex - 1
	end
	if targetIndex < 1 then
		return
	end
	TimeTravel.rewindToIndex(targetIndex)
end

function TimeTravel.stepForward()
	if not HISTORY.loaded or #HISTORY.entries == 0 then
		return
	end
	if HISTORY.currentIndex == 0 then
		return -- already live
	end
	local targetIndex: number = HISTORY.currentIndex + 1
	if targetIndex > #HISTORY.entries then
		-- Past the end of history: resume live
		TimeTravel.resumeLiveSync()
		return
	end
	TimeTravel.rewindToIndex(targetIndex)
end

function TimeTravel.jumpToOldest()
	if not HISTORY.loaded or #HISTORY.entries == 0 then
		return
	end
	TimeTravel.rewindToIndex(1)
end

function TimeTravel.jumpToLatest()
	TimeTravel.resumeLiveSync()
end

-- ─── Initial Builder Execution ──────────────────────────────────────────────

-- ─── DockWidget UI ──────────────────────────────────────────────────────────

local UI = {}
UI.WIDGET_ID = "VertigoSyncWidget"
UI.widget = plugin:CreateDockWidgetPluginGui(
	UI.WIDGET_ID,
	DockWidgetPluginGuiInfo.new(
		Enum.InitialDockState.Right,
		false, -- initially disabled
		true, -- override previous enabled state (remembers user's last open/close)
		340, -- default width
		400, -- default height
		300, -- min width
		300 -- min height
	)
)
UI.widget.Title = "Vertigo Sync"

-- ─── Settings Persistence ────────────────────────────────────────────────────

function Runtime.loadSettings()
	local binaryModels: any = plugin:GetSetting("VertigoSyncBinaryModels")
	if type(binaryModels) == "boolean" then
		SETTINGS.binaryModels = binaryModels
	end
	local builders: any = plugin:GetSetting("VertigoSyncBuildersEnabled")
	if type(builders) == "boolean" then
		SETTINGS.buildersEnabled = builders
	end
	local timeTravelUI: any = plugin:GetSetting("VertigoSyncTimeTravelUI")
	if type(timeTravelUI) == "boolean" then
		SETTINGS.timeTravelUI = timeTravelUI
	end
	local histBuf: any = plugin:GetSetting("VertigoSyncHistoryBuffer")
	if type(histBuf) == "number" and histBuf >= 16 and histBuf <= 1024 then
		SETTINGS.historyBuffer = math.floor(histBuf)
	end
	local applyQueueLimit: any = plugin:GetSetting("VertigoSyncApplyQueueLimit")
	if type(applyQueueLimit) == "number" and applyQueueLimit >= 0 then
		SETTINGS.applyQueueLimit = math.floor(applyQueueLimit)
	end
end

function Runtime.saveSetting(key: string, value: any)
	pcall(function()
		plugin:SetSetting(key, value)
	end)
end

Runtime.loadSettings()

-- ─── UI Design System ────────────────────────────────────────────────────────

UI.THEME_BG = Color3.fromRGB(30, 30, 30)
UI.THEME_SURFACE = Color3.fromRGB(38, 38, 38)
UI.THEME_SURFACE_ELEVATED = Color3.fromRGB(46, 46, 46)
UI.THEME_BORDER = Color3.fromRGB(60, 60, 60)
UI.THEME_TEXT = Color3.fromRGB(220, 220, 220)
UI.THEME_TEXT_DIM = Color3.fromRGB(140, 140, 140)
UI.THEME_ACCENT = Color3.fromRGB(56, 132, 244)
UI.THEME_GREEN = Color3.fromRGB(52, 199, 89)
UI.THEME_YELLOW = Color3.fromRGB(255, 159, 10)
UI.THEME_RED = Color3.fromRGB(255, 69, 58)

UI.TWEEN_FAST = TweenInfo.new(0.15, Enum.EasingStyle.Quad, Enum.EasingDirection.Out)
UI.TWEEN_MEDIUM = TweenInfo.new(0.2, Enum.EasingStyle.Quad, Enum.EasingDirection.Out)
UI.TWEEN_SLOW = TweenInfo.new(0.3, Enum.EasingStyle.Quad, Enum.EasingDirection.Out)
UI.TWEEN_POP = TweenInfo.new(0.25, Enum.EasingStyle.Back, Enum.EasingDirection.Out)
UI.TWEEN_PULSE = TweenInfo.new(3.2, Enum.EasingStyle.Sine, Enum.EasingDirection.InOut, -1, true)

UI.THEME_HOVER = Color3.fromRGB(
	math.min(255, math.floor(46 * 1.22)),
	math.min(255, math.floor(46 * 1.22)),
	math.min(255, math.floor(46 * 1.22))
)
UI.THEME_PRESS = Color3.fromRGB(
	math.max(0, math.floor(46 * 0.74)),
	math.max(0, math.floor(46 * 0.74)),
	math.max(0, math.floor(46 * 0.74))
)

function UI.createLabel(parent: Instance, name: string, text: string, props: {
	position: UDim2?,
	size: UDim2?,
	color: Color3?,
	fontSize: number?,
	font: Enum.Font?,
	xAlign: Enum.TextXAlignment?,
	layoutOrder: number?,
	wrap: boolean?,
	richText: boolean?,
}?): TextLabel
	local p = props or {}
	local label: TextLabel = Instance.new("TextLabel")
	label.Name = name
	label.Text = text
	label.Position = p.position or UDim2.new(0, 0, 0, 0)
	label.Size = p.size or UDim2.new(1, 0, 0, 16)
	label.BackgroundTransparency = 1
	label.TextColor3 = p.color or UI.THEME_TEXT
	label.TextSize = p.fontSize or 12
	label.Font = p.font or Enum.Font.RobotoMono
	label.TextXAlignment = p.xAlign or Enum.TextXAlignment.Left
	if p.wrap then
		label.TextWrapped = true
		label.AutomaticSize = Enum.AutomaticSize.Y
	else
		label.TextTruncate = Enum.TextTruncate.AtEnd
	end
	if p.richText then
		label.RichText = true
	end
	if p.layoutOrder then
		label.LayoutOrder = p.layoutOrder
	end
	label.Parent = parent
	return label
end

function UI.createPanel(parent: Instance, name: string, layoutOrder: number, height: number?): Frame
	local panel: Frame = Instance.new("Frame")
	panel.Name = name
	panel.Size = UDim2.new(1, 0, 0, height or 0)
	panel.AutomaticSize = if height then Enum.AutomaticSize.None else Enum.AutomaticSize.Y
	panel.BackgroundColor3 = UI.THEME_SURFACE
	panel.BorderSizePixel = 0
	panel.LayoutOrder = layoutOrder
	local corner: UICorner = Instance.new("UICorner")
	corner.CornerRadius = UDim.new(0, 6)
	corner.Parent = panel
	local stroke: UIStroke = Instance.new("UIStroke")
	stroke.Color = UI.THEME_BORDER
	stroke.Transparency = 0.4
	stroke.Thickness = 1
	stroke.Parent = panel
	local padding: UIPadding = Instance.new("UIPadding")
	padding.PaddingLeft = UDim.new(0, 12)
	padding.PaddingRight = UDim.new(0, 12)
	padding.PaddingTop = UDim.new(0, 8)
	padding.PaddingBottom = UDim.new(0, 8)
	padding.Parent = panel
	panel.Parent = parent
	return panel
end

function UI.createToggleSwitch(parent: Instance, name: string, labelText: string, initialState: boolean, layoutOrder: number): (Frame, Frame, TextLabel)
	local row: Frame = Instance.new("Frame")
	row.Name = name
	row.Size = UDim2.new(1, 0, 0, 24)
	row.BackgroundTransparency = 1
	row.LayoutOrder = layoutOrder
	row.Parent = parent

	local label: TextLabel = Instance.new("TextLabel")
	label.Name = "Label"
	label.Text = labelText
	label.Size = UDim2.new(1, -44, 1, 0)
	label.Position = UDim2.new(0, 0, 0, 0)
	label.BackgroundTransparency = 1
	label.TextColor3 = UI.THEME_TEXT
	label.TextSize = 12
	label.Font = Enum.Font.RobotoMono
	label.TextXAlignment = Enum.TextXAlignment.Left
	label.TextYAlignment = Enum.TextYAlignment.Center
	label.Parent = row

	local track: Frame = Instance.new("Frame")
	track.Name = "Track"
	track.Size = UDim2.new(0, 32, 0, 18)
	track.Position = UDim2.new(1, -32, 0.5, -9)
	track.BackgroundColor3 = if initialState then UI.THEME_ACCENT else UI.THEME_BG
	track.BorderSizePixel = 0
	local trackCorner: UICorner = Instance.new("UICorner")
	trackCorner.CornerRadius = UDim.new(1, 0)
	trackCorner.Parent = track
	local trackStroke: UIStroke = Instance.new("UIStroke")
	trackStroke.Color = UI.THEME_BORDER
	trackStroke.Transparency = 0.4
	trackStroke.Thickness = 1
	trackStroke.Parent = track
	track.Parent = row

	local thumbShadow: Frame = Instance.new("Frame")
	thumbShadow.Name = "ThumbShadow"
	thumbShadow.Size = UDim2.new(0, 14, 0, 14)
	thumbShadow.Position = if initialState then UDim2.new(1, -15, 0.5, -6) else UDim2.new(0, 3, 0.5, -6)
	thumbShadow.BackgroundColor3 = Color3.fromRGB(0, 0, 0)
	thumbShadow.BackgroundTransparency = 0.7
	thumbShadow.BorderSizePixel = 0
	local shadowCorner: UICorner = Instance.new("UICorner")
	shadowCorner.CornerRadius = UDim.new(1, 0)
	shadowCorner.Parent = thumbShadow
	thumbShadow.Parent = track

	local thumb: Frame = Instance.new("Frame")
	thumb.Name = "Thumb"
	thumb.Size = UDim2.new(0, 14, 0, 14)
	thumb.Position = if initialState then UDim2.new(1, -16, 0.5, -7) else UDim2.new(0, 2, 0.5, -7)
	thumb.BackgroundColor3 = Color3.fromRGB(255, 255, 255)
	thumb.BorderSizePixel = 0
	local thumbCorner: UICorner = Instance.new("UICorner")
	thumbCorner.CornerRadius = UDim.new(1, 0)
	thumbCorner.Parent = thumb
	thumb.Parent = track

	local clickBtn: TextButton = Instance.new("TextButton")
	clickBtn.Name = "ClickRegion"
	clickBtn.Size = UDim2.new(1, 0, 1, 0)
	clickBtn.BackgroundTransparency = 1
	clickBtn.Text = ""
	clickBtn.Parent = track

	return row, track, label
end

function UI.animateToggle(track: Frame, state: boolean)
	local thumb: Frame? = track:FindFirstChild("Thumb") :: Frame?
	local thumbShadow: Frame? = track:FindFirstChild("ThumbShadow") :: Frame?
	if thumb == nil then
		return
	end
	TweenService:Create(track, UI.TWEEN_FAST, {
		BackgroundColor3 = if state then UI.THEME_ACCENT else UI.THEME_BG,
	}):Play()
	TweenService:Create(thumb, UI.TWEEN_FAST, {
		Position = if state then UDim2.new(1, -16, 0.5, -7) else UDim2.new(0, 2, 0.5, -7),
	}):Play()
	if thumbShadow then
		TweenService:Create(thumbShadow, UI.TWEEN_FAST, {
			Position = if state then UDim2.new(1, -15, 0.5, -6) else UDim2.new(0, 3, 0.5, -6),
		}):Play()
	end
end

function UI.createSmallButton(parent: Instance, name: string, text: string, width: number): TextButton
	local btn: TextButton = Instance.new("TextButton")
	btn.Name = name
	btn.Text = text
	btn.Size = UDim2.new(0, width, 0, 22)
	btn.BackgroundColor3 = UI.THEME_SURFACE_ELEVATED
	btn.TextColor3 = UI.THEME_TEXT
	btn.TextSize = 11
	btn.Font = Enum.Font.RobotoMono
	btn.AutoButtonColor = false
	btn.BorderSizePixel = 0
	local corner: UICorner = Instance.new("UICorner")
	corner.CornerRadius = UDim.new(0, 4)
	corner.Parent = btn
	btn.MouseEnter:Connect(function()
		TweenService:Create(btn, UI.TWEEN_FAST, { BackgroundColor3 = UI.THEME_HOVER }):Play()
	end)
	btn.MouseLeave:Connect(function()
		TweenService:Create(btn, UI.TWEEN_FAST, { BackgroundColor3 = UI.THEME_SURFACE_ELEVATED }):Play()
	end)
	btn.MouseButton1Down:Connect(function()
		TweenService:Create(btn, UI.TWEEN_FAST, { BackgroundColor3 = UI.THEME_PRESS }):Play()
	end)
	btn.MouseButton1Up:Connect(function()
		TweenService:Create(btn, UI.TWEEN_FAST, { BackgroundColor3 = UI.THEME_SURFACE_ELEVATED }):Play()
	end)
	btn.Parent = parent
	return btn
end

function UI.buildToastSystem()
	local toastCount = 3
	local toastDismissSeconds = 2.0
	local toastHeight = 28
	local toastGap = 4

	local toastContainer: Frame = Instance.new("Frame")
	toastContainer.Name = "ToastContainer"
	toastContainer.Size = UDim2.new(1, -16, 0, (toastHeight + toastGap) * toastCount)
	toastContainer.Position = UDim2.new(0, 8, 1, -((toastHeight + toastGap) * toastCount) - 8)
	toastContainer.BackgroundTransparency = 1
	toastContainer.ZIndex = 10
	toastContainer.Parent = UI.widget

	local toastFrames: { Frame } = table.create(toastCount)
	local toastLabels: { TextLabel } = table.create(toastCount)
	local toastActive: { boolean } = table.create(toastCount, false)
	local toastDismissAt: { number } = table.create(toastCount, 0)
	local toastNextSlot = 1

	for i = 1, toastCount do
		local slot: Frame = Instance.new("Frame")
		slot.Name = "Toast" .. tostring(i)
		slot.Size = UDim2.new(1, 0, 0, toastHeight)
		slot.Position = UDim2.new(0, 0, 1, -(i * (toastHeight + toastGap)))
		slot.BackgroundColor3 = UI.THEME_SURFACE_ELEVATED
		slot.BackgroundTransparency = 1
		slot.BorderSizePixel = 0
		slot.ZIndex = 10
		slot.ClipsDescendants = true
		local slotCorner: UICorner = Instance.new("UICorner")
		slotCorner.CornerRadius = UDim.new(0, 6)
		slotCorner.Parent = slot
		local slotPad: UIPadding = Instance.new("UIPadding")
		slotPad.PaddingLeft = UDim.new(0, 10)
		slotPad.PaddingRight = UDim.new(0, 10)
		slotPad.Parent = slot

		local slotLabel: TextLabel = Instance.new("TextLabel")
		slotLabel.Name = "Label"
		slotLabel.Text = ""
		slotLabel.Size = UDim2.new(1, 0, 1, 0)
		slotLabel.BackgroundTransparency = 1
		slotLabel.TextColor3 = UI.THEME_TEXT
		slotLabel.TextSize = 11
		slotLabel.Font = Enum.Font.RobotoMono
		slotLabel.TextXAlignment = Enum.TextXAlignment.Left
		slotLabel.TextWrapped = true
		slotLabel.ZIndex = 10
		slotLabel.Parent = slot

		slot.Parent = toastContainer
		toastFrames[i] = slot
		toastLabels[i] = slotLabel
		toastActive[i] = false
		toastDismissAt[i] = 0
	end

	TOAST_COLOR_SUCCESS = UI.THEME_GREEN
	TOAST_COLOR_ERROR = UI.THEME_RED
	TOAST_COLOR_INFO = UI.THEME_ACCENT

	local lastToastMessage: string = ""
	local lastToastAt: number = 0

	showToast = function(message: string, toastColor: Color3?)
		local now: number = os.clock()
		if message == lastToastMessage and now - lastToastAt < 2.0 then
			return
		end
		lastToastMessage = message
		lastToastAt = now
		local color: Color3 = toastColor or TOAST_COLOR_INFO
		local slot: number = toastNextSlot
		toastNextSlot = (toastNextSlot % toastCount) + 1

		local frame: Frame = toastFrames[slot]
		local label: TextLabel = toastLabels[slot]

		label.Text = message
		label.TextTransparency = 0
		frame.BackgroundColor3 = color
		frame.BackgroundTransparency = 1
		frame.Position = UDim2.new(0, 0, 1, 0)

		local targetY: number = -(slot * (toastHeight + toastGap))
		TweenService:Create(frame, UI.TWEEN_POP, {
			BackgroundTransparency = 0.1,
			Position = UDim2.new(0, 0, 1, targetY),
		}):Play()

		toastActive[slot] = true
		toastDismissAt[slot] = os.clock() + toastDismissSeconds
	end

	task.spawn(function()
		while true do
			local now: number = os.clock()
			for i = 1, toastCount do
				if toastActive[i] and now >= toastDismissAt[i] then
					toastActive[i] = false
					TweenService:Create(toastFrames[i], UI.TWEEN_SLOW, {
						BackgroundTransparency = 1,
					}):Play()
					TweenService:Create(toastLabels[i], UI.TWEEN_SLOW, {
						TextTransparency = 1,
					}):Play()
					task.delay(0.35, function()
						if not toastActive[i] then
							toastLabels[i].Text = ""
						end
					end)
				end
			end
			task.wait(0.1)
		end
	end)
end

function UI.buildWelcomeSection(mainFrame: Frame)
	local welcomeFrame: Frame = Instance.new("Frame")
	welcomeFrame.Name = "WelcomeFrame"
	welcomeFrame.Size = UDim2.new(1, 0, 0, 0)
	welcomeFrame.AutomaticSize = Enum.AutomaticSize.Y
	welcomeFrame.BackgroundColor3 = UI.THEME_SURFACE
	welcomeFrame.BorderSizePixel = 0
	welcomeFrame.LayoutOrder = 0
	welcomeFrame.ClipsDescendants = true
	local welcomeCorner: UICorner = Instance.new("UICorner")
	welcomeCorner.CornerRadius = UDim.new(0, 6)
	welcomeCorner.Parent = welcomeFrame
	local welcomeStroke: UIStroke = Instance.new("UIStroke")
	welcomeStroke.Color = UI.THEME_BORDER
	welcomeStroke.Transparency = 0.4
	welcomeStroke.Thickness = 1
	welcomeStroke.Parent = welcomeFrame
	local welcomePad: UIPadding = Instance.new("UIPadding")
	welcomePad.PaddingLeft = UDim.new(0, 12)
	welcomePad.PaddingRight = UDim.new(0, 12)
	welcomePad.PaddingTop = UDim.new(0, 12)
	welcomePad.PaddingBottom = UDim.new(0, 12)
	welcomePad.Parent = welcomeFrame
	local welcomeLayout: UIListLayout = Instance.new("UIListLayout")
	welcomeLayout.SortOrder = Enum.SortOrder.LayoutOrder
	welcomeLayout.Padding = UDim.new(0, 8)
	welcomeLayout.Parent = welcomeFrame

	UI.createLabel(welcomeFrame, "WelcomeHeader", "Welcome to Vertigo Sync", {
		size = UDim2.new(1, 0, 0, 20),
		color = UI.THEME_TEXT,
		fontSize = 15,
		font = Enum.Font.GothamBold,
		layoutOrder = 1,
	})
	UI.createLabel(welcomeFrame, "Step1", "1. Install:", {
		size = UDim2.new(1, 0, 0, 14),
		color = UI.THEME_TEXT_DIM,
		fontSize = 12,
		font = Enum.Font.Gotham,
		layoutOrder = 2,
	})
	UI.createLabel(welcomeFrame, "Cmd1", "   cargo install vertigo-sync", {
		size = UDim2.new(1, 0, 0, 14),
		color = UI.THEME_ACCENT,
		fontSize = 11,
		font = Enum.Font.RobotoMono,
		layoutOrder = 3,
		wrap = true,
	})
	UI.createLabel(welcomeFrame, "Step2", "2. Start:", {
		size = UDim2.new(1, 0, 0, 14),
		color = UI.THEME_TEXT_DIM,
		fontSize = 12,
		font = Enum.Font.Gotham,
		layoutOrder = 4,
	})
	UI.createLabel(welcomeFrame, "Cmd2", "   vsync serve --turbo", {
		size = UDim2.new(1, 0, 0, 14),
		color = UI.THEME_ACCENT,
		fontSize = 11,
		font = Enum.Font.RobotoMono,
		layoutOrder = 5,
		wrap = true,
	})
	UI.createLabel(welcomeFrame, "Step3", "3. This panel connects automatically", {
		size = UDim2.new(1, 0, 0, 14),
		color = UI.THEME_TEXT_DIM,
		fontSize = 12,
		font = Enum.Font.Gotham,
		layoutOrder = 6,
		wrap = true,
	})
	local welcomeStep4: TextLabel = UI.createLabel(welcomeFrame, "Step4", "Click Check Connection once to trust the first server", {
		size = UDim2.new(1, 0, 0, 14),
		color = UI.THEME_TEXT_DIM,
		fontSize = 11,
		layoutOrder = 7,
		wrap = true,
	})
	welcomeStep4.FontFace = Font.new("rbxasset://fonts/families/Montserrat.json", Enum.FontWeight.Regular, Enum.FontStyle.Italic)

	local welcomeCheckBtn: TextButton = UI.createSmallButton(welcomeFrame, "CheckConnection", "Check Connection", 130)
	welcomeCheckBtn.LayoutOrder = 8

	local welcomeLearnMore: TextButton = Instance.new("TextButton")
	welcomeLearnMore.Name = "LearnMore"
	welcomeLearnMore.Text = "Learn more"
	welcomeLearnMore.Size = UDim2.new(0, 80, 0, 14)
	welcomeLearnMore.BackgroundTransparency = 1
	welcomeLearnMore.TextColor3 = UI.THEME_ACCENT
	welcomeLearnMore.TextSize = 11
	welcomeLearnMore.Font = Enum.Font.Gotham
	welcomeLearnMore.TextXAlignment = Enum.TextXAlignment.Left
	welcomeLearnMore.LayoutOrder = 9
	welcomeLearnMore.AutoButtonColor = false
	welcomeLearnMore.Parent = welcomeFrame

	welcomeFrame.Parent = mainFrame
	welcomeFrame.Visible = false

	return {
		welcomeFrame = welcomeFrame,
		welcomeCheckBtn = welcomeCheckBtn,
		welcomeLearnMore = welcomeLearnMore,
	}
end

function UI.buildStatusSection(mainFrame: Frame)
	local statusPanel: Frame = UI.createPanel(mainFrame, "StatusPanel", 1, 56)
	local statusDot: Frame = Instance.new("Frame")
	statusDot.Name = "StatusDot"
	statusDot.Size = UDim2.new(0, 6, 0, 6)
	statusDot.Position = UDim2.new(0, 0, 0, 4)
	statusDot.BackgroundColor3 = UI.THEME_YELLOW
	statusDot.BorderSizePixel = 0
	local dotCorner: UICorner = Instance.new("UICorner")
	dotCorner.CornerRadius = UDim.new(1, 0)
	dotCorner.Parent = statusDot
	statusDot.Parent = statusPanel

	local statusLine1: TextLabel = UI.createLabel(statusPanel, "StatusLine1", "Disconnected", {
		position = UDim2.new(0, 12, 0, 0),
		size = UDim2.new(1, -12, 0, 14),
		color = UI.THEME_TEXT,
		fontSize = 12,
		font = Enum.Font.GothamMedium,
	})
	local statusLine2: TextLabel = UI.createLabel(statusPanel, "StatusLine2", "Sync --  ·  0/s  ·  apply q0", {
		position = UDim2.new(0, 0, 0, 18),
		size = UDim2.new(1, 0, 0, 13),
		color = UI.THEME_TEXT_DIM,
		fontSize = 9,
		font = Enum.Font.RobotoMono,
	})
	local statusLine3: TextLabel = UI.createLabel(statusPanel, "StatusLine3", "Build ready  ·  Preview idle", {
		position = UDim2.new(0, 0, 0, 32),
		size = UDim2.new(1, 0, 0, 13),
		color = UI.THEME_TEXT_DIM,
		fontSize = 8,
		font = Enum.Font.RobotoMono,
	})

	return {
		statusDot = statusDot,
		statusLine1 = statusLine1,
		statusLine2 = statusLine2,
		statusLine3 = statusLine3,
	}
end

function UI.buildTogglesSection(mainFrame: Frame)
	local togglesPanel: Frame = UI.createPanel(mainFrame, "TogglesPanel", 2)
	local togglesLayout: UIListLayout = Instance.new("UIListLayout")
	togglesLayout.SortOrder = Enum.SortOrder.LayoutOrder
	togglesLayout.Padding = UDim.new(0, 4)
	togglesLayout.Parent = togglesPanel

	local _, binaryModelsTrack, _ = UI.createToggleSwitch(togglesPanel, "BinaryModelsToggle", "Binary Models", SETTINGS.binaryModels, 1)
	local _, buildersTrack, _ = UI.createToggleSwitch(togglesPanel, "BuildersToggle", "Builders", SETTINGS.buildersEnabled, 2)
	local _, timeTravelTrack, _ = UI.createToggleSwitch(togglesPanel, "TimeTravelToggle", "Time Travel", SETTINGS.timeTravelUI, 3)

	return {
		binaryModelsTrack = binaryModelsTrack,
		buildersTrack = buildersTrack,
		timeTravelTrack = timeTravelTrack,
	}
end

function UI.buildMainContainer()
	local scrollFrame: ScrollingFrame = Instance.new("ScrollingFrame")
	scrollFrame.Name = "ScrollContainer"
	scrollFrame.Size = UDim2.new(1, 0, 1, 0)
	scrollFrame.BackgroundColor3 = UI.THEME_BG
	scrollFrame.BorderSizePixel = 0
	scrollFrame.ScrollBarThickness = 3
	scrollFrame.ScrollBarImageColor3 = UI.THEME_BORDER
	scrollFrame.ScrollBarImageTransparency = 0.6
	scrollFrame.AutomaticCanvasSize = Enum.AutomaticSize.Y
	scrollFrame.CanvasSize = UDim2.new(0, 0, 0, 0)
	scrollFrame.Parent = UI.widget

	local mainFrame: Frame = Instance.new("Frame")
	mainFrame.Name = "MainFrame"
	mainFrame.Size = UDim2.new(1, 0, 0, 0)
	mainFrame.AutomaticSize = Enum.AutomaticSize.Y
	mainFrame.BackgroundColor3 = UI.THEME_BG
	mainFrame.BorderSizePixel = 0
	mainFrame.Parent = scrollFrame

	local mainPadding: UIPadding = Instance.new("UIPadding")
	mainPadding.PaddingLeft = UDim.new(0, 8)
	mainPadding.PaddingRight = UDim.new(0, 8)
	mainPadding.PaddingTop = UDim.new(0, 8)
	mainPadding.PaddingBottom = UDim.new(0, 8)
	mainPadding.Parent = mainFrame

	local mainLayout: UIListLayout = Instance.new("UIListLayout")
	mainLayout.SortOrder = Enum.SortOrder.LayoutOrder
	mainLayout.Padding = UDim.new(0, 8)
	mainLayout.Parent = mainFrame

	return {
		scrollFrame = scrollFrame,
		mainFrame = mainFrame,
	}
end

function UI.packWidgetRefs(welcomeRefs, statusRefs, toggleRefs, timeTravelRefs, settingsRefs)
	return {
		welcomeFrame = welcomeRefs.welcomeFrame,
		welcomeCheckBtn = welcomeRefs.welcomeCheckBtn,
		welcomeLearnMore = welcomeRefs.welcomeLearnMore,
		statusDot = statusRefs.statusDot,
		statusLine1 = statusRefs.statusLine1,
		statusLine2 = statusRefs.statusLine2,
		statusLine3 = statusRefs.statusLine3,
		binaryModelsTrack = toggleRefs.binaryModelsTrack,
		buildersTrack = toggleRefs.buildersTrack,
		timeTravelTrack = toggleRefs.timeTravelTrack,
		timeTravelPanel = timeTravelRefs.timeTravelPanel,
		btnJumpOldest = timeTravelRefs.btnJumpOldest,
		btnStepBack = timeTravelRefs.btnStepBack,
		ttLiveDot = timeTravelRefs.ttLiveDot,
		ttSeqLabel = timeTravelRefs.ttSeqLabel,
		ttStatusLabel = timeTravelRefs.ttStatusLabel,
		btnStepFwd = timeTravelRefs.btnStepFwd,
		btnJumpLatest = timeTravelRefs.btnJumpLatest,
		scrubberFill = timeTravelRefs.scrubberFill,
		scrubberHitbox = timeTravelRefs.scrubberHitbox,
		scrubberTrack = timeTravelRefs.scrubberTrack,
		scrubberThumb = timeTravelRefs.scrubberThumb,
		scrubberThumbShadow = timeTravelRefs.scrubberThumbShadow,
		historyListFrame = timeTravelRefs.historyListFrame,
		historyRowCount = timeTravelRefs.historyRowCount,
		historyRowFrames = timeTravelRefs.historyRowFrames,
		historyRowTimeLabels = timeTravelRefs.historyRowTimeLabels,
		historyRowAddedLabels = timeTravelRefs.historyRowAddedLabels,
		historyRowModifiedLabels = timeTravelRefs.historyRowModifiedLabels,
		historyRowDeletedLabels = timeTravelRefs.historyRowDeletedLabels,
		retryHistoryBtn = timeTravelRefs.retryHistoryBtn,
		settingsTogglePanel = settingsRefs.settingsTogglePanel,
		settingsPanel = settingsRefs.settingsPanel,
		settingsHeaderBtn = settingsRefs.settingsHeaderBtn,
		settingsHeaderArrow = settingsRefs.settingsHeaderArrow,
		historyBufferLabel = settingsRefs.historyBufferLabel,
		historyBufferValueLabel = settingsRefs.historyBufferValueLabel,
		historyBufferDecreaseBtn = settingsRefs.historyBufferDecreaseBtn,
		historyBufferIncreaseBtn = settingsRefs.historyBufferIncreaseBtn,
		historyBufferRefreshBtn = settingsRefs.historyBufferRefreshBtn,
		applyQueueLabel = settingsRefs.applyQueueLabel,
		applyQueueValueLabel = settingsRefs.applyQueueValueLabel,
		applyQueueDecreaseBtn = settingsRefs.applyQueueDecreaseBtn,
		applyQueueIncreaseBtn = settingsRefs.applyQueueIncreaseBtn,
		applyQueueUnlimitedBtn = settingsRefs.applyQueueUnlimitedBtn,
		settingsToggleBtn = settingsRefs.settingsToggleBtn,
		settingsToggleLabel = settingsRefs.settingsToggleLabel,
		settingsToggleArrow = settingsRefs.settingsToggleArrow,
		statusPulseTween = nil,
		lastStatusLine1Text = "",
		lastStatusLine1Color = nil,
		lastStatusLine2Text = "",
		lastStatusLine3Text = "",
		lastStatusLine3Color = nil,
		lastTimelineStatusText = "",
		lastTimelineStatusColor = nil,
		lastRetryHistoryVisible = nil,
		lastTimeTravelDisplayKey = "",
		lastHistoryRowTexts = timeTravelRefs.lastHistoryRowTexts,
		lastHistoryRowColors = timeTravelRefs.lastHistoryRowColors,
	}
end

function UI.buildWidgetSections(mainFrame: Frame)
	local welcomeRefs = UI.buildWelcomeSection(mainFrame)
	local statusRefs = UI.buildStatusSection(mainFrame)
	local toggleRefs = UI.buildTogglesSection(mainFrame)
	local timeTravelRefs = UI.buildTimeTravelSection(mainFrame)
	local settingsRefs = UI.buildSettingsSection(mainFrame)
	return UI.packWidgetRefs(welcomeRefs, statusRefs, toggleRefs, timeTravelRefs, settingsRefs)
end

function UI.buildWidgetUi()
	local rootRefs = UI.buildMainContainer()
	UI.buildToastSystem()
	return UI.buildWidgetSections(rootRefs.mainFrame)
end

function UI.buildTimeTravelSection(mainFrame: Frame)
	local timeTravelPanel: Frame = UI.createPanel(mainFrame, "TimeTravelPanel", 3, 214)
	timeTravelPanel.Visible = SETTINGS.timeTravelUI
	local inset = TIME_TRAVEL_LIST_BOTTOM_PADDING
	local scrubberInset = 6

	local ttStatsRow: Frame = Instance.new("Frame")
	ttStatsRow.Name = "StatsRow"
	ttStatsRow.Size = UDim2.new(1, -(inset * 2), 0, 20)
	ttStatsRow.Position = UDim2.new(0, inset, 0, 0)
	ttStatsRow.BackgroundTransparency = 1
	ttStatsRow.Parent = timeTravelPanel

	local ttStatusLabel: TextLabel = UI.createLabel(ttStatsRow, "StatusLabel", "Time Travel", {
		position = UDim2.new(0, 0, 0, 0),
		size = UDim2.new(1, -56, 1, 0),
		color = UI.THEME_TEXT,
		fontSize = 12,
		font = Enum.Font.GothamBold,
	})
	local ttSeqLabel: TextLabel = UI.createLabel(ttStatsRow, "SeqLabel", "", {
		position = UDim2.new(1, -56, 0, 0),
		size = UDim2.new(0, 56, 1, 0),
		color = UI.THEME_ACCENT,
		fontSize = 9,
		font = Enum.Font.RobotoMono,
		xAlign = Enum.TextXAlignment.Right,
	})
	local retryHistoryBtn: TextButton = UI.createSmallButton(ttStatsRow, "RetryHistory", "Retry", 52)
	retryHistoryBtn.Position = UDim2.new(1, -52, 0, 0)
	retryHistoryBtn.Visible = false

	local ttNavRow: Frame = Instance.new("Frame")
	ttNavRow.Name = "NavRow"
	ttNavRow.Size = UDim2.new(1, -(inset * 2), 0, 24)
	ttNavRow.Position = UDim2.new(0, inset, 0, 24)
	ttNavRow.BackgroundTransparency = 1
	ttNavRow.Parent = timeTravelPanel

	local navGap = 4
	local leftW = 24 + navGap + 20
	local rightW = 20 + navGap + 24

	local ttNavLeft: Frame = Instance.new("Frame")
	ttNavLeft.Name = "LeftControls"
	ttNavLeft.Size = UDim2.new(0, leftW, 1, 0)
	ttNavLeft.BackgroundTransparency = 1
	ttNavLeft.Parent = ttNavRow
	local btnJumpOldest: TextButton = UI.createSmallButton(ttNavLeft, "JumpOldest", "|<", 24)
	btnJumpOldest.Position = UDim2.new(0, 0, 0.5, -11)
	local btnStepBack: TextButton = UI.createSmallButton(ttNavLeft, "StepBack", "<", 20)
	btnStepBack.Position = UDim2.new(0, 24 + navGap, 0.5, -11)

	local ttLiveDot: Frame = Instance.new("Frame")
	ttLiveDot.Name = "StatusDot"
	ttLiveDot.Size = UDim2.new(0, 8, 0, 8)
	ttLiveDot.AnchorPoint = Vector2.new(0.5, 0.5)
	ttLiveDot.Position = UDim2.new(0.5, 0, 0.5, 0)
	ttLiveDot.BackgroundColor3 = UI.THEME_GREEN
	ttLiveDot.BorderSizePixel = 0
	local ttLiveDotCorner: UICorner = Instance.new("UICorner")
	ttLiveDotCorner.CornerRadius = UDim.new(1, 0)
	ttLiveDotCorner.Parent = ttLiveDot
	ttLiveDot.Parent = ttNavRow

	local ttNavRight: Frame = Instance.new("Frame")
	ttNavRight.Name = "RightControls"
	ttNavRight.Size = UDim2.new(0, rightW, 1, 0)
	ttNavRight.Position = UDim2.new(1, -rightW, 0, 0)
	ttNavRight.BackgroundTransparency = 1
	ttNavRight.Parent = ttNavRow
	local btnStepFwd: TextButton = UI.createSmallButton(ttNavRight, "StepFwd", ">", 20)
	btnStepFwd.Position = UDim2.new(1, -(20 + navGap + 24), 0.5, -11)
	local btnJumpLatest: TextButton = UI.createSmallButton(ttNavRight, "JumpLatest", ">|", 24)
	btnJumpLatest.Position = UDim2.new(1, -24, 0.5, -11)

	local scrubberContainer: Frame = Instance.new("Frame")
	scrubberContainer.Name = "ScrubberContainer"
	scrubberContainer.Size = UDim2.new(1, -(scrubberInset * 2), 0, 16)
	scrubberContainer.Position = UDim2.new(0, scrubberInset, 0, 54)
	scrubberContainer.BackgroundTransparency = 1
	scrubberContainer.Parent = timeTravelPanel

	local scrubberTrack: Frame = Instance.new("Frame")
	scrubberTrack.Name = "Track"
	scrubberTrack.Size = UDim2.new(1, 0, 0, 4)
	scrubberTrack.Position = UDim2.new(0, 0, 0.5, -2)
	scrubberTrack.BackgroundColor3 = UI.THEME_SURFACE_ELEVATED
	scrubberTrack.BorderSizePixel = 0
	local scrubberTrackCorner: UICorner = Instance.new("UICorner")
	scrubberTrackCorner.CornerRadius = UDim.new(1, 0)
	scrubberTrackCorner.Parent = scrubberTrack
	scrubberTrack.Parent = scrubberContainer

	local scrubberHitbox: TextButton = Instance.new("TextButton")
	scrubberHitbox.Name = "Hitbox"
	scrubberHitbox.Size = UDim2.new(1, 0, 1, 0)
	scrubberHitbox.BackgroundTransparency = 1
	scrubberHitbox.Text = ""
	scrubberHitbox.AutoButtonColor = false
	scrubberHitbox.ZIndex = 2
	scrubberHitbox.Parent = scrubberContainer

	for i = 1, 5 do
		local tick: Frame = Instance.new("Frame")
		tick.Name = "Tick" .. tostring(i)
		tick.Size = UDim2.new(0, 1, 0, 6)
		tick.AnchorPoint = Vector2.new(0.5, 0)
		tick.Position = UDim2.new((i - 1) / 4, 0, 0.5, 4)
		tick.BackgroundColor3 = UI.THEME_BORDER
		tick.BackgroundTransparency = if i == 1 or i == 5 then 0.2 else 0.45
		tick.BorderSizePixel = 0
		tick.Parent = scrubberContainer
	end

	local scrubberFill: Frame = Instance.new("Frame")
	scrubberFill.Name = "Fill"
	scrubberFill.Size = UDim2.new(1, 0, 1, 0)
	scrubberFill.BackgroundColor3 = Color3.fromRGB(72, 148, 255)
	scrubberFill.BorderSizePixel = 0
	local fillCorner: UICorner = Instance.new("UICorner")
	fillCorner.CornerRadius = UDim.new(1, 0)
	fillCorner.Parent = scrubberFill
	scrubberFill.Parent = scrubberTrack

	local scrubberThumbShadow: Frame = Instance.new("Frame")
	scrubberThumbShadow.Name = "ThumbShadow"
	scrubberThumbShadow.Size = UDim2.new(0, 14, 0, 14)
	scrubberThumbShadow.Position = UDim2.new(1, -6, 0.5, -6)
	scrubberThumbShadow.AnchorPoint = Vector2.new(0.5, 0.5)
	scrubberThumbShadow.BackgroundColor3 = Color3.fromRGB(0, 0, 0)
	scrubberThumbShadow.BackgroundTransparency = 0.7
	scrubberThumbShadow.BorderSizePixel = 0
	local thumbShadowCorner: UICorner = Instance.new("UICorner")
	thumbShadowCorner.CornerRadius = UDim.new(1, 0)
	thumbShadowCorner.Parent = scrubberThumbShadow
	scrubberThumbShadow.Parent = scrubberContainer

	local scrubberThumb: Frame = Instance.new("Frame")
	scrubberThumb.Name = "Thumb"
	scrubberThumb.Size = UDim2.new(0, 14, 0, 14)
	scrubberThumb.Position = UDim2.new(1, -7, 0.5, -7)
	scrubberThumb.AnchorPoint = Vector2.new(0.5, 0.5)
	scrubberThumb.BackgroundColor3 = UI.THEME_ACCENT
	scrubberThumb.BorderSizePixel = 0
	local thumbCorner: UICorner = Instance.new("UICorner")
	thumbCorner.CornerRadius = UDim.new(1, 0)
	thumbCorner.Parent = scrubberThumb
	scrubberThumb.Parent = scrubberContainer

	local historyHeaderRow: Frame = Instance.new("Frame")
	historyHeaderRow.Name = "HistoryHeader"
	historyHeaderRow.Size = UDim2.new(1, -(inset * 2), 0, 14)
	historyHeaderRow.Position = UDim2.new(0, inset, 0, 76)
	historyHeaderRow.BackgroundTransparency = 1
	historyHeaderRow.Parent = timeTravelPanel

	local timeX, timeW = 0.03, 0.38
	local addX, addW = 0.50, 0.12
	local modX, modW = 0.67, 0.12
	local delX, delW = 0.84, 0.12

	local historyHeaderTime: TextLabel = UI.createLabel(historyHeaderRow, "Time", "time", {
		position = UDim2.new(timeX, 0, 0, 0),
		size = UDim2.new(timeW, 0, 1, 0),
		color = UI.THEME_TEXT_DIM,
		fontSize = 9,
		font = Enum.Font.RobotoMono,
	})
	historyHeaderTime.TextXAlignment = Enum.TextXAlignment.Left
	UI.createLabel(historyHeaderRow, "Added", "add", { position = UDim2.new(addX, 0, 0, 0), size = UDim2.new(addW, 0, 1, 0), color = UI.THEME_TEXT_DIM, fontSize = 9, font = Enum.Font.RobotoMono, xAlign = Enum.TextXAlignment.Right })
	UI.createLabel(historyHeaderRow, "Modified", "mod", { position = UDim2.new(modX, 0, 0, 0), size = UDim2.new(modW, 0, 1, 0), color = UI.THEME_TEXT_DIM, fontSize = 9, font = Enum.Font.RobotoMono, xAlign = Enum.TextXAlignment.Right })
	UI.createLabel(historyHeaderRow, "Deleted", "del", { position = UDim2.new(delX, 0, 0, 0), size = UDim2.new(delW, 0, 1, 0), color = UI.THEME_TEXT_DIM, fontSize = 9, font = Enum.Font.RobotoMono, xAlign = Enum.TextXAlignment.Right })

	local historyListFrame: ScrollingFrame = Instance.new("ScrollingFrame")
	historyListFrame.Name = "HistoryList"
	historyListFrame.Size = UDim2.new(1, -(inset * 2), 0, 120)
	historyListFrame.Position = UDim2.new(0, inset, 0, 92)
	historyListFrame.BackgroundTransparency = 1
	historyListFrame.BorderSizePixel = 0
	historyListFrame.Active = true
	historyListFrame.ScrollingEnabled = true
	historyListFrame.ScrollBarThickness = 3
	historyListFrame.ScrollBarImageColor3 = UI.THEME_BORDER
	historyListFrame.ScrollBarImageTransparency = 0.6
	historyListFrame.AutomaticCanvasSize = Enum.AutomaticSize.None
	historyListFrame.CanvasSize = UDim2.new(0, 0, 0, 0)
	historyListFrame.ScrollingDirection = Enum.ScrollingDirection.Y
	historyListFrame.ClipsDescendants = true
	historyListFrame.Parent = timeTravelPanel
	local historyListPadding = Instance.new("UIPadding")
	historyListPadding.PaddingBottom = UDim.new(0, inset)
	historyListPadding.Parent = historyListFrame

	local historyRowCount = 24
	local historyRowFrames = table.create(historyRowCount)
	local historyRowTimeLabels = table.create(historyRowCount)
	local historyRowAddedLabels = table.create(historyRowCount)
	local historyRowModifiedLabels = table.create(historyRowCount)
	local historyRowDeletedLabels = table.create(historyRowCount)
	for i = 1, historyRowCount do
		local rowFrame: Frame = Instance.new("Frame")
		local baseRowColor = if i % 2 == 1 then UI.THEME_SURFACE else UI.THEME_BG
		local baseRowTransparency = if i % 2 == 1 then 0.6 else 1
		rowFrame.Name = "RowFrame" .. tostring(i)
		rowFrame.Size = UDim2.new(1, 0, 0, 21)
		rowFrame.Position = UDim2.new(0, 0, 0, (i - 1) * 23)
		rowFrame.BackgroundColor3 = baseRowColor
		rowFrame.BackgroundTransparency = baseRowTransparency
		rowFrame.BorderSizePixel = 0
		rowFrame:SetAttribute("HistoryEntryIndex", nil)
		rowFrame:SetAttribute("HistoryVisualIndex", i)
		local rowCorner: UICorner = Instance.new("UICorner")
		rowCorner.CornerRadius = UDim.new(0, 3)
		rowCorner.Parent = rowFrame
		rowFrame.Parent = historyListFrame

		local rowTimeLabel: TextLabel = UI.createLabel(rowFrame, "Time", "", { size = UDim2.new(timeW, 0, 1, 0), position = UDim2.new(timeX, 0, 0, 0), color = UI.THEME_TEXT_DIM, fontSize = 9, font = Enum.Font.RobotoMono })
		rowTimeLabel.TextXAlignment = Enum.TextXAlignment.Left
		local rowAddedLabel: TextLabel = UI.createLabel(rowFrame, "Added", "", { size = UDim2.new(addW, 0, 1, 0), position = UDim2.new(addX, 0, 0, 0), color = UI.THEME_TEXT_DIM, fontSize = 9, font = Enum.Font.RobotoMono, xAlign = Enum.TextXAlignment.Right })
		local rowModifiedLabel: TextLabel = UI.createLabel(rowFrame, "Modified", "", { size = UDim2.new(modW, 0, 1, 0), position = UDim2.new(modX, 0, 0, 0), color = UI.THEME_TEXT_DIM, fontSize = 9, font = Enum.Font.RobotoMono, xAlign = Enum.TextXAlignment.Right })
		local rowDeletedLabel: TextLabel = UI.createLabel(rowFrame, "Deleted", "", { size = UDim2.new(delW, 0, 1, 0), position = UDim2.new(delX, 0, 0, 0), color = UI.THEME_TEXT_DIM, fontSize = 9, font = Enum.Font.RobotoMono, xAlign = Enum.TextXAlignment.Right })

		local rowBtn: TextButton = Instance.new("TextButton")
		rowBtn.Name = "HoverRegion"
		rowBtn.Size = UDim2.new(1, 0, 1, 0)
		rowBtn.BackgroundTransparency = 1
		rowBtn.Text = ""
		rowBtn.Parent = rowFrame
		rowBtn.MouseEnter:Connect(function()
			TweenService:Create(rowFrame, UI.TWEEN_FAST, { BackgroundTransparency = 0, BackgroundColor3 = UI.THEME_SURFACE_ELEVATED }):Play()
		end)
		rowBtn.MouseLeave:Connect(function()
			local rowIdx = rowFrame:GetAttribute("HistoryEntryIndex")
			local visualIndex = rowFrame:GetAttribute("HistoryVisualIndex")
			local isOddRow = if typeof(visualIndex) == "number" then (visualIndex % 2 == 1) else (i % 2 == 1)
			local isSelected = rowIdx == HISTORY.currentIndex and HISTORY.currentIndex > 0
			TweenService:Create(rowFrame, UI.TWEEN_FAST, {
				BackgroundTransparency = if isSelected then 0.15 else (if isOddRow then 0.6 else 1),
				BackgroundColor3 = if isSelected then UI.THEME_SURFACE_ELEVATED else (if isOddRow then UI.THEME_SURFACE else UI.THEME_BG),
			}):Play()
		end)
		rowBtn.MouseButton1Click:Connect(function()
			if HISTORY.fetchFailed or HISTORY.busy then return end
			local entryCount: number = #HISTORY.entries
			local rowIdx = rowFrame:GetAttribute("HistoryEntryIndex")
			if rowIdx < 1 or rowIdx > entryCount then return end
			TimeTravel.rewindToIndex(rowIdx)
		end)

		historyRowFrames[i] = rowFrame
		historyRowTimeLabels[i] = rowTimeLabel
		historyRowAddedLabels[i] = rowAddedLabel
		historyRowModifiedLabels[i] = rowModifiedLabel
		historyRowDeletedLabels[i] = rowDeletedLabel
	end

	return {
		timeTravelPanel = timeTravelPanel,
		btnJumpOldest = btnJumpOldest,
		btnStepBack = btnStepBack,
		ttLiveDot = ttLiveDot,
		ttSeqLabel = ttSeqLabel,
		ttStatusLabel = ttStatusLabel,
		btnStepFwd = btnStepFwd,
		btnJumpLatest = btnJumpLatest,
		scrubberFill = scrubberFill,
		scrubberHitbox = scrubberHitbox,
		scrubberTrack = scrubberTrack,
		scrubberThumb = scrubberThumb,
		scrubberThumbShadow = scrubberThumbShadow,
		historyListFrame = historyListFrame,
		historyRowCount = historyRowCount,
		historyRowFrames = historyRowFrames,
		historyRowTimeLabels = historyRowTimeLabels,
		historyRowAddedLabels = historyRowAddedLabels,
		historyRowModifiedLabels = historyRowModifiedLabels,
		historyRowDeletedLabels = historyRowDeletedLabels,
		retryHistoryBtn = retryHistoryBtn,
		lastHistoryRowTexts = table.create(historyRowCount, ""),
		lastHistoryRowColors = table.create(historyRowCount),
	}
end

function UI.buildSettingsSection(mainFrame: Frame)
	local settingsTogglePanel: Frame = UI.createPanel(mainFrame, "SettingsTogglePanel", 4, 24)
	settingsTogglePanel.ClipsDescendants = true
	local settingsToggleBtn: TextButton = Instance.new("TextButton")
	settingsToggleBtn.Name = "SettingsToggleBtn"
	settingsToggleBtn.Text = ""
	settingsToggleBtn.Size = UDim2.new(1, 0, 1, 0)
	settingsToggleBtn.BackgroundTransparency = 1
	settingsToggleBtn.AutoButtonColor = false
	settingsToggleBtn.Parent = settingsTogglePanel
	local settingsToggleLabel: TextLabel = Instance.new("TextLabel")
	settingsToggleLabel.Name = "Label"
	settingsToggleLabel.Text = "Settings"
	settingsToggleLabel.Size = UDim2.new(1, -20, 1, 0)
	settingsToggleLabel.BackgroundTransparency = 1
	settingsToggleLabel.TextColor3 = UI.THEME_TEXT
	settingsToggleLabel.TextSize = 13
	settingsToggleLabel.Font = Enum.Font.GothamBold
	settingsToggleLabel.TextXAlignment = Enum.TextXAlignment.Left
	settingsToggleLabel.Parent = settingsTogglePanel
	local settingsToggleArrow: TextLabel = Instance.new("TextLabel")
	settingsToggleArrow.Name = "Arrow"
	settingsToggleArrow.Text = "v"
	settingsToggleArrow.Position = UDim2.new(1, -16, 0, 0)
	settingsToggleArrow.Size = UDim2.new(0, 16, 1, 0)
	settingsToggleArrow.BackgroundTransparency = 1
	settingsToggleArrow.TextColor3 = UI.THEME_TEXT_DIM
	settingsToggleArrow.TextSize = 12
	settingsToggleArrow.Font = Enum.Font.GothamBold
	settingsToggleArrow.TextXAlignment = Enum.TextXAlignment.Right
	settingsToggleArrow.Parent = settingsTogglePanel

	local settingsPanel: Frame = UI.createPanel(mainFrame, "SettingsPanel", 5)
	settingsPanel.Visible = false
	settingsPanel.ClipsDescendants = true
	local settingsHeaderRow: Frame = Instance.new("Frame")
	settingsHeaderRow.Name = "HeaderRow"
	settingsHeaderRow.Size = UDim2.new(1, 0, 0, 20)
	settingsHeaderRow.BackgroundColor3 = UI.THEME_SURFACE
	settingsHeaderRow.LayoutOrder = 0
	local settingsHeaderCorner: UICorner = Instance.new("UICorner")
	settingsHeaderCorner.CornerRadius = UDim.new(0, 4)
	settingsHeaderCorner.Parent = settingsHeaderRow
	settingsHeaderRow.Parent = settingsPanel
	local settingsHeaderBtn: TextButton = Instance.new("TextButton")
	settingsHeaderBtn.Name = "HeaderBtn"
	settingsHeaderBtn.Text = ""
	settingsHeaderBtn.Size = UDim2.new(1, 0, 1, 0)
	settingsHeaderBtn.BackgroundTransparency = 1
	settingsHeaderBtn.AutoButtonColor = false
	settingsHeaderBtn.Parent = settingsHeaderRow
	local settingsHeaderTitle: TextLabel = UI.createLabel(settingsHeaderRow, "Title", "Settings", {
		position = UDim2.new(0, 0, 0, 0),
		size = UDim2.new(1, -20, 0, 20),
		color = UI.THEME_TEXT,
		fontSize = 13,
		font = Enum.Font.GothamBold,
	})
	local settingsHeaderArrow: TextLabel = Instance.new("TextLabel")
	settingsHeaderArrow.Name = "Arrow"
	settingsHeaderArrow.Text = "^"
	settingsHeaderArrow.Position = UDim2.new(1, -16, 0, 0)
	settingsHeaderArrow.Size = UDim2.new(0, 16, 1, 0)
	settingsHeaderArrow.BackgroundTransparency = 1
	settingsHeaderArrow.TextColor3 = UI.THEME_TEXT_DIM
	settingsHeaderArrow.TextSize = 12
	settingsHeaderArrow.Font = Enum.Font.GothamBold
	settingsHeaderArrow.TextXAlignment = Enum.TextXAlignment.Right
	settingsHeaderArrow.Parent = settingsHeaderRow

	local function bindDisclosureFeedback(button: TextButton, frame: Frame, label: TextLabel, arrow: TextLabel)
		local function applyState(bgColor: Color3, labelColor: Color3, arrowColor: Color3)
			TweenService:Create(frame, UI.TWEEN_FAST, { BackgroundColor3 = bgColor }):Play()
			TweenService:Create(label, UI.TWEEN_FAST, { TextColor3 = labelColor }):Play()
			TweenService:Create(arrow, UI.TWEEN_FAST, { TextColor3 = arrowColor }):Play()
		end
		button.MouseEnter:Connect(function() applyState(UI.THEME_HOVER, UI.THEME_TEXT, UI.THEME_TEXT) end)
		button.MouseLeave:Connect(function() applyState(UI.THEME_SURFACE, UI.THEME_TEXT, UI.THEME_TEXT_DIM) end)
		button.MouseButton1Down:Connect(function() applyState(UI.THEME_PRESS, UI.THEME_TEXT, UI.THEME_TEXT) end)
		button.MouseButton1Up:Connect(function() applyState(UI.THEME_HOVER, UI.THEME_TEXT, UI.THEME_TEXT) end)
	end
	bindDisclosureFeedback(settingsToggleBtn, settingsTogglePanel, settingsToggleLabel, settingsToggleArrow)
	bindDisclosureFeedback(settingsHeaderBtn, settingsHeaderRow, settingsHeaderTitle, settingsHeaderArrow)

	UI.createLabel(settingsPanel, "Subtitle", "Sync is automatic. These settings are local and optional.", {
		color = UI.THEME_TEXT_DIM,
		fontSize = 10,
		font = Enum.Font.Gotham,
		layoutOrder = 1,
		wrap = true,
	})
	local settingsLayout: UIListLayout = Instance.new("UIListLayout")
	settingsLayout.SortOrder = Enum.SortOrder.LayoutOrder
	settingsLayout.Padding = UDim.new(0, 4)
	settingsLayout.Parent = settingsPanel
	local leftBankWidth = 76
	local rowInset = 4

	local historyBufferGroup: Frame = Instance.new("Frame")
	historyBufferGroup.Name = "HistoryBufferGroup"
	historyBufferGroup.Size = UDim2.new(1, 0, 0, 36)
	historyBufferGroup.BackgroundTransparency = 1
	historyBufferGroup.LayoutOrder = 10
	historyBufferGroup.Parent = settingsPanel
	local historyBufferLabel: TextLabel = UI.createLabel(historyBufferGroup, "HistoryBufferLabel", "Timeline depth", {
		position = UDim2.new(0, 0, 0, 0),
		size = UDim2.new(1, 0, 0, 12),
		color = UI.THEME_TEXT_DIM,
		fontSize = 10,
		font = Enum.Font.Gotham,
	})
	local historyBufferRow: Frame = Instance.new("Frame")
	historyBufferRow.Name = "HistoryBufferRow"
	historyBufferRow.Size = UDim2.new(1, 0, 0, 22)
	historyBufferRow.Position = UDim2.new(0, 0, 0, 14)
	historyBufferRow.BackgroundTransparency = 1
	historyBufferRow.Parent = historyBufferGroup
	local historyBufferLeftControls: Frame = Instance.new("Frame")
	historyBufferLeftControls.Size = UDim2.new(0, leftBankWidth, 1, 0)
	historyBufferLeftControls.Position = UDim2.new(0, rowInset, 0, 0)
	historyBufferLeftControls.BackgroundTransparency = 1
	historyBufferLeftControls.Parent = historyBufferRow
	local historyBufferLeftLayout: UIListLayout = Instance.new("UIListLayout")
	historyBufferLeftLayout.FillDirection = Enum.FillDirection.Horizontal
	historyBufferLeftLayout.HorizontalAlignment = Enum.HorizontalAlignment.Left
	historyBufferLeftLayout.SortOrder = Enum.SortOrder.LayoutOrder
	historyBufferLeftLayout.Padding = UDim.new(0, 3)
	historyBufferLeftLayout.VerticalAlignment = Enum.VerticalAlignment.Center
	historyBufferLeftLayout.Parent = historyBufferLeftControls
	local historyBufferDecreaseBtn: TextButton = UI.createSmallButton(historyBufferLeftControls, "HistoryBufferDecrease", "-", 18)
	historyBufferDecreaseBtn.LayoutOrder = 1
	local historyBufferValueLabel: TextLabel = UI.createLabel(historyBufferLeftControls, "HistoryBufferValue", tostring(SETTINGS.historyBuffer), { size = UDim2.new(0, 34, 0, 22), color = UI.THEME_TEXT, fontSize = 11, font = Enum.Font.RobotoMono, xAlign = Enum.TextXAlignment.Center, layoutOrder = 2 })
	local historyBufferIncreaseBtn: TextButton = UI.createSmallButton(historyBufferLeftControls, "HistoryBufferIncrease", "+", 18)
	historyBufferIncreaseBtn.LayoutOrder = 3
	local historyBufferRefreshBtn: TextButton = UI.createSmallButton(historyBufferRow, "HistoryBufferRefresh", "Reload", 38)
	historyBufferRefreshBtn.Position = UDim2.new(1, -(38 + rowInset), 0, 0)

	local applyQueueGroup: Frame = Instance.new("Frame")
	applyQueueGroup.Name = "ApplyQueueGroup"
	applyQueueGroup.Size = UDim2.new(1, 0, 0, 36)
	applyQueueGroup.BackgroundTransparency = 1
	applyQueueGroup.LayoutOrder = 11
	applyQueueGroup.Parent = settingsPanel
	local applyQueueLabel: TextLabel = UI.createLabel(applyQueueGroup, "ApplyQueueLabel", "Apply queue limit", {
		position = UDim2.new(0, 0, 0, 0),
		size = UDim2.new(1, 0, 0, 12),
		color = UI.THEME_TEXT_DIM,
		fontSize = 10,
		font = Enum.Font.Gotham,
	})
	local applyQueueRow: Frame = Instance.new("Frame")
	applyQueueRow.Name = "ApplyQueueRow"
	applyQueueRow.Size = UDim2.new(1, 0, 0, 22)
	applyQueueRow.Position = UDim2.new(0, 0, 0, 14)
	applyQueueRow.BackgroundTransparency = 1
	applyQueueRow.Parent = applyQueueGroup
	local applyQueueLeftControls: Frame = Instance.new("Frame")
	applyQueueLeftControls.Size = UDim2.new(0, leftBankWidth, 1, 0)
	applyQueueLeftControls.Position = UDim2.new(0, rowInset, 0, 0)
	applyQueueLeftControls.BackgroundTransparency = 1
	applyQueueLeftControls.Parent = applyQueueRow
	local applyQueueLeftLayout: UIListLayout = Instance.new("UIListLayout")
	applyQueueLeftLayout.FillDirection = Enum.FillDirection.Horizontal
	applyQueueLeftLayout.HorizontalAlignment = Enum.HorizontalAlignment.Left
	applyQueueLeftLayout.SortOrder = Enum.SortOrder.LayoutOrder
	applyQueueLeftLayout.Padding = UDim.new(0, 3)
	applyQueueLeftLayout.VerticalAlignment = Enum.VerticalAlignment.Center
	applyQueueLeftLayout.Parent = applyQueueLeftControls
	local applyQueueDecreaseBtn: TextButton = UI.createSmallButton(applyQueueLeftControls, "ApplyQueueDecrease", "-", 18)
	applyQueueDecreaseBtn.LayoutOrder = 1
	local applyQueueValueLabel: TextLabel = UI.createLabel(applyQueueLeftControls, "ApplyQueueValue", "∞", { size = UDim2.new(0, 30, 0, 22), color = UI.THEME_TEXT, fontSize = 11, font = Enum.Font.RobotoMono, xAlign = Enum.TextXAlignment.Center, layoutOrder = 2 })
	local applyQueueIncreaseBtn: TextButton = UI.createSmallButton(applyQueueLeftControls, "ApplyQueueIncrease", "+", 18)
	applyQueueIncreaseBtn.LayoutOrder = 3
	local applyQueueUnlimitedBtn: TextButton = UI.createSmallButton(applyQueueRow, "ApplyQueueUnlimited", "Uncap", 40)
	applyQueueUnlimitedBtn.Position = UDim2.new(1, -(40 + rowInset), 0, 0)

	return {
		settingsTogglePanel = settingsTogglePanel,
		settingsPanel = settingsPanel,
		settingsHeaderBtn = settingsHeaderBtn,
		settingsHeaderArrow = settingsHeaderArrow,
		historyBufferLabel = historyBufferLabel,
		historyBufferValueLabel = historyBufferValueLabel,
		historyBufferDecreaseBtn = historyBufferDecreaseBtn,
		historyBufferIncreaseBtn = historyBufferIncreaseBtn,
		historyBufferRefreshBtn = historyBufferRefreshBtn,
		applyQueueLabel = applyQueueLabel,
		applyQueueValueLabel = applyQueueValueLabel,
		applyQueueDecreaseBtn = applyQueueDecreaseBtn,
		applyQueueIncreaseBtn = applyQueueIncreaseBtn,
		applyQueueUnlimitedBtn = applyQueueUnlimitedBtn,
		settingsToggleBtn = settingsToggleBtn,
		settingsToggleLabel = settingsToggleLabel,
		settingsToggleArrow = settingsToggleArrow,
	}
end

UI.refs = UI.buildWidgetUi()

function UI.updateHistoryBufferUI()
	local refs = UI.refs
	refs.historyBufferLabel.Text = "Timeline depth"
	refs.historyBufferValueLabel.Text = tostring(SETTINGS.historyBuffer)
end

function UI.updateApplyQueueLimitUI()
	local refs = UI.refs
	refs.applyQueueLabel.Text = "Apply queue limit"
	if SETTINGS.applyQueueLimit <= 0 then
		refs.applyQueueValueLabel.Text = "∞"
	else
		refs.applyQueueValueLabel.Text = tostring(SETTINGS.applyQueueLimit)
	end
end

function UI.getTrackClickRegion(track: Frame): TextButton?
	return track:FindFirstChild("ClickRegion") :: TextButton?
end

function UI.setSettingsPanelVisible(isVisible: boolean)
	local refs = UI.refs
	refs.settingsPanel.Visible = isVisible
	refs.settingsTogglePanel.Visible = not isVisible
	refs.settingsToggleArrow.Text = "v"
	refs.settingsHeaderArrow.Text = "^"
end

	function UI.bindTimeTravelScrubberHandlers()
		local refs = UI.refs
		local userInputService = game:GetService("UserInputService")
		local scrubberDragging = false

	local function scrubberSeekFromAbsoluteX(absoluteX: number)
		if HISTORY.busy or HISTORY.fetchFailed or not HISTORY.loaded then
			return
		end
		local entryCount = #HISTORY.entries
		if entryCount == 0 then
			return
		end
		local trackPos = refs.scrubberTrack.AbsolutePosition.X
		local trackSize = refs.scrubberTrack.AbsoluteSize.X
		if trackSize <= 0 then
			return
		end
		local ratio = math.clamp((absoluteX - trackPos) / trackSize, 0, 1)
		local liveHotzonePx = 2
		if absoluteX >= (trackPos + trackSize - liveHotzonePx) then
			TimeTravel.resumeLiveSync()
			return
		end
		local targetIndex = math.clamp(math.floor((ratio * math.max(entryCount - 1, 0)) + 0.5) + 1, 1, entryCount)
		TimeTravel.rewindToIndex(targetIndex)
	end

	refs.scrubberHitbox.MouseButton1Down:Connect(function(x: number, _y: number)
		scrubberDragging = true
		scrubberSeekFromAbsoluteX(x)
	end)

	refs.scrubberHitbox.MouseButton1Up:Connect(function(x: number, _y: number)
		scrubberDragging = false
		scrubberSeekFromAbsoluteX(x)
	end)

		refs.scrubberHitbox.MouseMoved:Connect(function(x: number, _y: number)
			if scrubberDragging then
				scrubberSeekFromAbsoluteX(x)
			end
		end)

		userInputService.InputChanged:Connect(function(input: InputObject)
			if not scrubberDragging then
				return
			end
			if input.UserInputType == Enum.UserInputType.MouseMovement or input.UserInputType == Enum.UserInputType.Touch then
				scrubberSeekFromAbsoluteX(input.Position.X)
			end
		end)

	userInputService.InputEnded:Connect(function(input: InputObject)
		if input.UserInputType == Enum.UserInputType.MouseButton1 or input.UserInputType == Enum.UserInputType.Touch then
			scrubberDragging = false
		end
	end)
end

-- Everything below runs inside a function to create a new register scope.
-- Luau has a 200 local register limit per function; do/end does NOT help.
-- Keep the entrypoint on an existing table so we do not burn another top-level local register.
function UI._initPlugin()

runInitialBuilders = function()
	if not BUILDERS.enabled then
		return
	end
	if not isEditMode() then
		return
	end
	local syncFingerprint: string = lastHash or ""
	if syncFingerprint ~= "" then
		if BUILDERS.initialPending > 0 and BUILDERS.lastInitialQueuedFingerprint == syncFingerprint then
			return
		end
		if
			BUILDERS.lastInitialCompletedFingerprint == syncFingerprint
			and not Runtime.hasQueuedBuilders()
			and not BUILDERS.pumpActive
		then
			return
		end
	end
	if #PROJECT.builderRoots == 0 then
		info("Builders enabled, but /project declared no vertigoSync.builder roots.")
		return
	end
	local waitDeadline = os.clock() + 10
	while os.clock() < waitDeadline do
		local pendingDepth: number = math.max(#pendingQueue - pendingQueueHead + 1, 0)
		local fetchDepth: number = math.max(#fetchQueue - fetchQueueHead + 1, 0)
		if pendingDepth == 0 and fetchDepth == 0 and fetchInFlight == 0 then
			break
		end
		task.wait(0.1)
	end

	-- Safety: skip initial builder execution if Workspace already has baked
	-- geometry from the .rbxl. This prevents duplicate world content.
	-- Incremental rebuilds (triggered by file changes) still work normally
	-- because they clean up tagged output before re-running.
	local bakedModelCount: number = 0
	for _, child: Instance in ipairs(Workspace:GetChildren()) do
		if child:IsA("Model") and child.Name ~= "Camera" then
			bakedModelCount += 1
		end
	end
	if bakedModelCount >= 3 then
		info(string.format("Baked world detected (%d models). Checking for stale builders...", bakedModelCount))
		-- Signal ZoneService to skip builders on Play
		Workspace:SetAttribute("VertigoSyncWorldReady", true)

		-- Restore BUILDERS.outputTags from persisted attributes on baked Models.
		-- This enables stale detection across Studio restarts without CollectionService tags.
		local restoredCount: number = 0
		for _, child: Instance in ipairs(Workspace:GetChildren()) do
			if child:IsA("Model") then
				local buildHash: string? = child:GetAttribute("VertigoBuildHash")
				local buildPath: string? = child:GetAttribute("VertigoBuildPath")
				if buildHash ~= nil and buildPath ~= nil then
					local segments: { string } = string.split(buildPath, "/")
					local joined: string = table.concat(segments, "_")
					local parts: { string } = string.split(joined, ".")
					local sanitized: string = table.concat(parts, "_")
					local restoredTag: string = "BuilderOutput_" .. sanitized .. "_" .. string.sub(buildHash, 1, 8)
					if BUILDERS.outputTags[buildPath] == nil then
						BUILDERS.outputTags[buildPath] = restoredTag
						restoredCount += 1
					end
				end
			end
		end
		if restoredCount > 0 then
			info(string.format("Restored %d builder output tags from baked attributes.", restoredCount))
		end

		-- Check each builder for stale output: if the source hash changed
		-- since the baked output was created, rebuild ONLY that builder.
		-- This ensures source code changes (like floor thickness) are applied
		-- to the baked world without rebuilding everything.
		local staleCount: number = 0
		local builderPathCount: number = #PROJECT.builderRoots
		for i = 1, builderPathCount do
			local builderPrefix: string = PROJECT.builderRoots[i]
			for path: string, inst: Instance in pairs(managedIndex) do
				if string.sub(path, 1, #builderPrefix) == builderPrefix and inst:IsA("LuaSourceContainer") then
					Runtime.computeBuilderDependencies(path, inst :: LuaSourceContainer)
					-- Check if this builder's output hash matches current source
					local currentSha: string = inst:GetAttribute(CORE.MANAGED_SHA_ATTR) or ""
					local lastBuiltTag: string = BUILDERS.outputTags[path] or ""
					if currentSha ~= "" and lastBuiltTag == "" then
						-- No record of previous build — check if any tagged output exists
						-- Use string.split to build expected tag prefix
						local segments: { string } = string.split(path, "/")
						local joined: string = table.concat(segments, "_")
						local parts: { string } = string.split(joined, ".")
						local sanitized: string = table.concat(parts, "_")
						local expectedTag: string = "BuilderOutput_" .. sanitized .. "_" .. string.sub(currentSha, 1, 8)
						local existingOutputs: { Instance } = CollectionService:GetTagged(expectedTag)
						if #existingOutputs > 0 then
							-- Output exists with matching hash — skip
							BUILDERS.outputTags[path] = expectedTag
						else
							-- Source changed since baked output — rebuild this builder
							staleCount += 1
							BUILDERS.dirtySet[path] = true
						end
					end
				end
			end
		end
		if staleCount > 0 then
			info(string.format("Found %d stale builders — scheduling incremental rebuild.", staleCount))
			BUILDERS.scheduleBatch()
		else
			info("All builders current — no rebuilds needed.")
		end
		return
	end

	local totalStart: number = os.clock()
	local builderList: { { path: string, inst: LuaSourceContainer } } = {}

	local builderPathCount: number = #PROJECT.builderRoots
	for i = 1, builderPathCount do
		local builderPrefix: string = PROJECT.builderRoots[i]
		for path: string, inst: Instance in pairs(managedIndex) do
			if string.sub(path, 1, #builderPrefix) == builderPrefix then
				if inst:IsA("LuaSourceContainer") then
					table.insert(builderList, { path = path, inst = inst :: LuaSourceContainer })
				end
			end
		end
	end

	-- Sort: HubBuilder first (spawn zone), then alphabetical for determinism
	table.sort(builderList, function(a: { path: string, inst: LuaSourceContainer }, b: { path: string, inst: LuaSourceContainer }): boolean
		local aIsHub: boolean = string.find(a.path, "HubBuilder", 1, true) ~= nil
		local bIsHub: boolean = string.find(b.path, "HubBuilder", 1, true) ~= nil
		if aIsHub ~= bIsHub then
			return aIsHub
		end
		return a.path < b.path
	end)

	BUILDERS.initialPending = #builderList
	BUILDERS.initialExecuted = 0
	BUILDERS.initialSkipped = 0
	BUILDERS.initialStartedAt = totalStart
	BUILDERS.lastInitialQueuedFingerprint = syncFingerprint

	for _, entry: { path: string, inst: LuaSourceContainer } in builderList do
		Runtime.enqueueBuilderPath(entry.path)
	end

	info(string.format("Initial builders queued: %d", #builderList))
	Runtime.scheduleBuilderPump()
end

-- ─── Health / transport ─────────────────────────────────────────────────────

@native
function Runtime.checkHealth(): boolean
	local ok, payloadOrErr = requestJson("/health")
	if ok then
		consecutiveErrors = 0
		-- Detect server restart via stable server_id; server_boot_time is elapsed
		-- uptime and will change on every poll.
		if type(payloadOrErr) == "table" then
			local reportedServerId: any = payloadOrErr.server_id
			if type(reportedServerId) == "string" and reportedServerId ~= "" then
				if serverIdCache ~= nil and reportedServerId ~= serverIdCache then
					throttledLog(
						"server_restart",
						string.format("Server restart detected (server_id %s -> %s), requesting resync", serverIdCache :: string, reportedServerId),
						false
					)
					resyncRequested = true
					PROJECT.loaded = false
					setProjectStatus("bootstrapping", "Refreshing /project after server restart", PROJECT.name, false)
				end
				serverIdCache = reportedServerId
			elseif type(payloadOrErr.server_boot_time) == "number" then
				local reportedBootTime: number = payloadOrErr.server_boot_time
				if serverBootTimeCache ~= nil and reportedBootTime < (serverBootTimeCache :: number) then
					throttledLog(
						"server_restart",
						string.format("Server restart detected (boot_time %d -> %d), requesting resync", serverBootTimeCache :: number, reportedBootTime),
						false
					)
					resyncRequested = true
					PROJECT.loaded = false
					setProjectStatus("bootstrapping", "Refreshing /project after server restart", PROJECT.name, false)
				end
				serverBootTimeCache = reportedBootTime
			end
		end
		return true
	end
	consecutiveErrors += 1
	throttledLog("health_fail", string.format("Health check failed (attempt=%d): %s", consecutiveErrors, tostring(payloadOrErr)), true)
	return false
end

function Runtime.closeWebSocket(reason: string)
	if wsSocket ~= nil then
		local socket = wsSocket
		wsSocket = nil
		pcall(function()
			socket:Close()
		end)
	end
	if wsConnected then
		info(string.format("WebSocket disconnected (%s)", reason))
	end
	wsConnected = false
	if transportMode == "ws" then
		transportMode = "poll"
	end
end

function Runtime.scheduleWsReconnect()
	local now = os.clock()
	nextWsConnectAt = now + wsReconnectBackoffSeconds + math.random() * 0.15
	wsReconnectBackoffSeconds = math.min(CORE.WS_RECONNECT_MAX_SECONDS, wsReconnectBackoffSeconds * 1.7)
end

@native
function Runtime.onWsMessage(rawText: string)
	local decodeOk, payloadOrErr = pcall(function()
		return HttpService:JSONDecode(rawText)
	end)
	if not decodeOk then
		warnMsg(string.format("WS JSON decode failed: %s", tostring(payloadOrErr)))
		return
	end

	local payload = payloadOrErr :: any
	if type(payload) ~= "table" then
		return
	end

	local messageType = payload.type
	if type(messageType) ~= "string" then
		return
	end

	if messageType == "connected" then
		transportMode = "ws"
		wsConnected = true
		wsReconnectBackoffSeconds = CORE.WS_RECONNECT_MIN_SECONDS

		local fingerprint = payload.fingerprint
		if type(fingerprint) == "string" and fingerprint ~= "" then
			if lastHash == nil or lastHash ~= fingerprint then
				resyncRequested = true
			else
				setStatusAttributes("connected", fingerprint)
			end
		end
		return
	end

	if messageType == "lagged" then
		laggedEvents += 1
		resyncRequested = true
		warnMsg("WS client lagged; requesting snapshot resync")
		return
	end

	if messageType == "sync_diff" then
		local sourceHash = payload.source_hash
		if type(sourceHash) == "string" and sourceHash ~= "" then
			lastHash = sourceHash
			setStatusAttributes("connected", sourceHash)
		end

		local paths = payload.paths
		if type(paths) == "table" then
			Runtime.stagePaths(paths.added, "write")
			Runtime.stagePaths(paths.modified, "write")
			Runtime.stagePaths(paths.deleted, "delete")
			-- Process renames: move instance in-place instead of delete+recreate
			if type(paths.renamed) == "table" then
				for _, entry in ipairs(paths.renamed) do
					if type(entry) == "table" and type(entry.old_path) == "string" and type(entry.new_path) == "string" then
						Runtime.stageRename(entry.old_path, entry.new_path, nil)
					end
				end
			end
		end
	end
end

function Runtime.tryConnectWebSocket()
	if WebSocketService == nil then
		return false
	end
	if wsConnected or wsSocket ~= nil then
		return true
	end
	if os.clock() < nextWsConnectAt then
		return false
	end

	local wsUrl = wsUrlFromHttpBase(getServerBaseUrl())
	local ok, socketOrErr = pcall(function()
		return (WebSocketService :: any):ConnectAsync(wsUrl)
	end)
	if not ok then
		transportMode = "poll"
		Runtime.scheduleWsReconnect()
		warnMsg(string.format("WS connect failed (%s); falling back to polling", tostring(socketOrErr)))
		return false
	end

	local socket = socketOrErr
	wsSocket = socket
	transportMode = "ws"
	reconnectCount += 1

	(socket :: any).MessageReceived:Connect(function(message: string)
		Runtime.onWsMessage(message)
	end)

	if (socket :: any).Closed ~= nil then
		(socket :: any).Closed:Connect(function()
			Runtime.closeWebSocket("closed")
			Runtime.scheduleWsReconnect()
		end)
	end

	info("WebSocket connected: realtime streaming enabled")
	return true
end

-- ─── UI Event Handlers ──────────────────────────────────────────────────────

function UI.bindHandlers()
	local ui = UI.refs

	UI.updateHistoryBufferUI()
	UI.updateApplyQueueLimitUI()

	-- Toggle switches
	local binaryModelsClickRegion = UI.getTrackClickRegion(ui.binaryModelsTrack)
	if binaryModelsClickRegion then
		binaryModelsClickRegion.MouseButton1Click:Connect(function()
			SETTINGS.binaryModels = not SETTINGS.binaryModels
			UI.animateToggle(ui.binaryModelsTrack, SETTINGS.binaryModels)
			Runtime.saveSetting("VertigoSyncBinaryModels", SETTINGS.binaryModels)
			Workspace:SetAttribute("VertigoSyncBinaryModels", SETTINGS.binaryModels)
		end)
	end

	local buildersClickRegion = UI.getTrackClickRegion(ui.buildersTrack)
	if buildersClickRegion then
		buildersClickRegion.MouseButton1Click:Connect(function()
			SETTINGS.buildersEnabled = not SETTINGS.buildersEnabled
			UI.animateToggle(ui.buildersTrack, SETTINGS.buildersEnabled)
			BUILDERS.enabled = SETTINGS.buildersEnabled and isEditMode()
			Runtime.saveSetting("VertigoSyncBuildersEnabled", SETTINGS.buildersEnabled)
			Workspace:SetAttribute("VertigoSyncBuildersEnabled", BUILDERS.enabled)
			if BUILDERS.enabled and currentStatus == "connected" then
				task.defer(runInitialBuilders)
			end
		end)
	end

	local timeTravelClickRegion = UI.getTrackClickRegion(ui.timeTravelTrack)
	if timeTravelClickRegion then
		timeTravelClickRegion.MouseButton1Click:Connect(function()
			SETTINGS.timeTravelUI = not SETTINGS.timeTravelUI
			UI.animateToggle(ui.timeTravelTrack, SETTINGS.timeTravelUI)
			ui.timeTravelPanel.Visible = SETTINGS.timeTravelUI
			publishTimeTravelAttributes()
			Runtime.saveSetting("VertigoSyncTimeTravelUI", SETTINGS.timeTravelUI)
		end)
	end

	UI.setSettingsPanelVisible(ui.settingsPanel.Visible)

	ui.settingsToggleBtn.MouseButton1Click:Connect(function()
		UI.setSettingsPanelVisible(not ui.settingsPanel.Visible)
	end)

	ui.settingsHeaderBtn.MouseButton1Click:Connect(function()
		UI.setSettingsPanelVisible(not ui.settingsPanel.Visible)
	end)

	ui.historyBufferDecreaseBtn.MouseButton1Click:Connect(function()
		local nextValue = math.max(16, SETTINGS.historyBuffer - 16)
		if nextValue == SETTINGS.historyBuffer then
			return
		end
		SETTINGS.historyBuffer = nextValue
		Runtime.saveSetting("VertigoSyncHistoryBuffer", SETTINGS.historyBuffer)
		UI.updateHistoryBufferUI()
	end)

	ui.historyBufferIncreaseBtn.MouseButton1Click:Connect(function()
		local nextValue = math.min(1024, SETTINGS.historyBuffer + 16)
		if nextValue == SETTINGS.historyBuffer then
			return
		end
		SETTINGS.historyBuffer = nextValue
		Runtime.saveSetting("VertigoSyncHistoryBuffer", SETTINGS.historyBuffer)
		UI.updateHistoryBufferUI()
	end)

	ui.historyBufferRefreshBtn.MouseButton1Click:Connect(function()
		HISTORY.fetchFailed = false
		TimeTravel.fetchHistory(true)
	end)

	ui.applyQueueDecreaseBtn.MouseButton1Click:Connect(function()
		local nextValue = SETTINGS.applyQueueLimit
		if nextValue <= 0 then
			nextValue = APPLY.QUEUE_HIGH_WATERMARK
		else
			nextValue = math.max(0, nextValue - 256)
		end
		SETTINGS.applyQueueLimit = nextValue
		Runtime.saveSetting("VertigoSyncApplyQueueLimit", SETTINGS.applyQueueLimit)
		UI.updateApplyQueueLimitUI()
	end)

	ui.applyQueueIncreaseBtn.MouseButton1Click:Connect(function()
		local nextValue = if SETTINGS.applyQueueLimit <= 0 then APPLY.QUEUE_HIGH_WATERMARK else SETTINGS.applyQueueLimit + 256
		SETTINGS.applyQueueLimit = nextValue
		Runtime.saveSetting("VertigoSyncApplyQueueLimit", SETTINGS.applyQueueLimit)
		UI.updateApplyQueueLimitUI()
	end)

	ui.applyQueueUnlimitedBtn.MouseButton1Click:Connect(function()
		if SETTINGS.applyQueueLimit == 0 then
			return
		end
		SETTINGS.applyQueueLimit = 0
		Runtime.saveSetting("VertigoSyncApplyQueueLimit", SETTINGS.applyQueueLimit)
		UI.updateApplyQueueLimitUI()
	end)

	-- Time-travel button handlers
	ui.btnJumpOldest.MouseButton1Click:Connect(function()
		if HISTORY.fetchFailed then
			return
		end
		if not HISTORY.loaded then
			TimeTravel.fetchHistory(true)
		end
		TimeTravel.jumpToOldest()
	end)

	ui.btnStepBack.MouseButton1Click:Connect(function()
		if HISTORY.fetchFailed then
			return
		end
		if not HISTORY.loaded then
			TimeTravel.fetchHistory(true)
		end
		TimeTravel.stepBackward()
	end)

	ui.btnStepFwd.MouseButton1Click:Connect(function()
		TimeTravel.stepForward()
	end)

	ui.btnJumpLatest.MouseButton1Click:Connect(function()
		TimeTravel.jumpToLatest()
	end)

	UI.bindTimeTravelScrubberHandlers()

	-- Retry history button
	ui.retryHistoryBtn.MouseButton1Click:Connect(function()
		HISTORY.fetchFailed = false
		ui.retryHistoryBtn.Visible = false
		TimeTravel.fetchHistory(true)
	end)

	-- Welcome screen: "Check Connection" triggers immediate health check
	ui.welcomeCheckBtn.MouseButton1Click:Connect(function()
		connectionState = "connecting"
		SERVER.allowUntrustedDiscovery = true
		local bootstrapOk = ensureProjectBootstrap(true)
		local healthOk = bootstrapOk and Runtime.checkHealth()
		SERVER.allowUntrustedDiscovery = false
		if healthOk then
			hasEverConnected = true
			connectionState = "connected"
			-- Fade out welcome screen
			TweenService:Create(ui.welcomeFrame, UI.TWEEN_SLOW, { BackgroundTransparency = 1 }):Play()
			task.delay(0.3, function()
				ui.welcomeFrame.Visible = false
				ui.welcomeFrame.BackgroundTransparency = 0
			end)
			resyncRequested = true
		else
			connectionState = "error"
			local message = if PROJECT.blocked and PROJECT.message ~= ""
				then PROJECT.message
				else string.format("Server not reachable at %s", getServerBaseUrl())
			showToast(message, TOAST_COLOR_ERROR)
		end
	end)

	-- Welcome screen: "Learn more" opens documentation URL
	ui.welcomeLearnMore.MouseButton1Click:Connect(function()
		-- Cannot open URL from plugin; set attribute for external tooling
		Workspace:SetAttribute("VertigoSyncDocsRequest", "https://github.com/vertigo-sync/vertigo-sync")
		showToast("Docs: github.com/vertigo-sync", TOAST_COLOR_INFO)
	end)

	-- Workspace attribute toggles for external control
	Workspace:GetAttributeChangedSignal("VertigoSyncBinaryModels"):Connect(function()
		local val: any = Workspace:GetAttribute("VertigoSyncBinaryModels")
		if type(val) == "boolean" then
			SETTINGS.binaryModels = val
			UI.animateToggle(ui.binaryModelsTrack, SETTINGS.binaryModels)
		end
	end)

	Workspace:GetAttributeChangedSignal("VertigoSyncBuildersEnabled"):Connect(function()
		local val: any = Workspace:GetAttribute("VertigoSyncBuildersEnabled")
		if type(val) == "boolean" then
			SETTINGS.buildersEnabled = val
			BUILDERS.enabled = SETTINGS.buildersEnabled and isEditMode()
			UI.animateToggle(ui.buildersTrack, SETTINGS.buildersEnabled)
		end
	end)
end

UI.bindHandlers()

-- ─── UI Status Refresh ──────────────────────────────────────────────────────

local lastConnectionStateForUI: ConnectionState = "waiting"

function Runtime.refreshStatusUI()
	local ui = UI.refs
	-- ─── Connection state machine → visual state mapping ─────────────────
	-- Update connectionState based on currentStatus and reconnect info
	if currentStatus == "connected" then
		if not hasEverConnected then
			hasEverConnected = true
			-- Fade out welcome screen on first successful connection
			if ui.welcomeFrame.Visible then
				TweenService:Create(ui.welcomeFrame, UI.TWEEN_SLOW, { BackgroundTransparency = 1 }):Play()
				task.delay(0.3, function()
					ui.welcomeFrame.Visible = false
					ui.welcomeFrame.BackgroundTransparency = 0
				end)
			end
		end
		connectionState = "connected"
		connectionReconnectAttempt = 0
	elseif currentStatus == "error" then
		connectionState = "error"
	elseif currentStatus == "disconnected" then
		if hasEverConnected then
			connectionState = "reconnecting"
			connectionReconnectAttempt = consecutiveErrors
		else
			connectionState = "waiting"
		end
	end

	-- Show welcome frame only when never-connected and health fails
	ui.welcomeFrame.Visible = not hasEverConnected and connectionState ~= "connected"

	-- Status line 1: connection indicator with dot
	local statusText: string
	local dotColor: Color3
	local line1Color: Color3 = UI.THEME_TEXT
	if connectionState == "connected" then
		statusText = "Connected"
		dotColor = UI.THEME_GREEN
	elseif connectionState == "reconnecting" then
		statusText = string.format("Reconnecting %d", connectionReconnectAttempt)
		dotColor = UI.THEME_YELLOW
	elseif connectionState == "connecting" then
		statusText = "Connecting..."
		dotColor = UI.THEME_ACCENT
	elseif connectionState == "error" then
		local errDetail: string
		if PROJECT.blocked then
			errDetail = PROJECT.message
		else
			errDetail = if consecutiveErrors > 0
				then string.format("Health check failed (%d)", consecutiveErrors)
				else "Connection error"
		end
		statusText = errDetail
		dotColor = UI.THEME_RED
		line1Color = UI.THEME_RED
	else -- "waiting"
		statusText = "Waiting for server"
		dotColor = UI.THEME_ACCENT
	end

	local nextStatusLine1Text = statusText
	if ui.lastStatusLine1Text ~= nextStatusLine1Text then
		ui.lastStatusLine1Text = nextStatusLine1Text
		ui.statusLine1.Text = nextStatusLine1Text
	end
	if ui.lastStatusLine1Color ~= line1Color then
		ui.lastStatusLine1Color = line1Color
		ui.statusLine1.TextColor3 = line1Color
	end
	if ui.statusDot.BackgroundColor3 ~= dotColor then
		TweenService:Create(ui.statusDot, UI.TWEEN_SLOW, { BackgroundColor3 = dotColor }):Play()
	end

	-- Manage pulse tween based on connection state
	if connectionState ~= lastConnectionStateForUI then
		lastConnectionStateForUI = connectionState
		if ui.statusPulseTween then
			ui.statusPulseTween:Cancel()
			ui.statusPulseTween = nil
		end
		if connectionState == "connecting" or connectionState == "reconnecting" then
			ui.statusDot.BackgroundTransparency = 0.2
			ui.statusPulseTween = TweenService:Create(ui.statusDot, UI.TWEEN_PULSE, { BackgroundTransparency = 0.55 })
			ui.statusPulseTween:Play()
		elseif connectionState == "waiting" then
			ui.statusDot.BackgroundTransparency = 0.35
		elseif connectionState == "connected" then
			ui.statusDot.BackgroundTransparency = 0
		else
			ui.statusDot.BackgroundTransparency = 0
		end
	end

	-- Status line 2: transport + sync apply telemetry
	local queueDepth: number = math.max(#pendingQueue - pendingQueueHead + 1, 0)
	local transportLabel: string = if transportMode == "ws" then "ws" elseif transportMode == "poll" then "poll" else "--"
	local nextStatusLine2Text = string.format("%s %d/s q%d", transportLabel, appliedPerSecond, queueDepth)
	if ui.lastStatusLine2Text ~= nextStatusLine2Text then
		ui.lastStatusLine2Text = nextStatusLine2Text
		ui.statusLine2.Text = nextStatusLine2Text
	end

	-- Status line 3: builder + preview telemetry
	local nextStatusLine3Text = string.format("%s · %s", PERF.builderStatusSummary(), PERF.previewStatusSummary())
	if ui.lastStatusLine3Text ~= nextStatusLine3Text then
		ui.lastStatusLine3Text = nextStatusLine3Text
		ui.statusLine3.Text = nextStatusLine3Text
	end
	local nextStatusLine3Color: Color3
	local previewState: any = Workspace:GetAttribute("VertigoPreviewSyncState")
	if PROJECT.mode == "mismatch" then
		nextStatusLine3Color = UI.THEME_RED
	elseif previewState == "running" or previewState == "scheduled" or BUILDERS.pumpActive then
		nextStatusLine3Color = UI.THEME_ACCENT
	elseif Workspace:GetAttribute("VertigoSyncBuilderLastResult") == "failed" or previewState == "failed" or previewState == "error" then
		nextStatusLine3Color = UI.THEME_RED
	else
		nextStatusLine3Color = UI.THEME_TEXT_DIM
	end
	if ui.lastStatusLine3Color ~= nextStatusLine3Color then
		ui.lastStatusLine3Color = nextStatusLine3Color
		ui.statusLine3.TextColor3 = nextStatusLine3Color
	end

	-- Time-travel panel
	if SETTINGS.timeTravelUI then
		local function setTimeTravelTimelineVisuals(fillScale: number)
			TweenService:Create(ui.scrubberFill, UI.TWEEN_FAST, { Size = UDim2.new(fillScale, 0, 1, 0) }):Play()
			TweenService:Create(ui.scrubberThumb, UI.TWEEN_FAST, { Position = UDim2.new(fillScale, 0, 0.5, 0) }):Play()
			TweenService:Create(ui.scrubberThumbShadow, UI.TWEEN_FAST, { Position = UDim2.new(fillScale, 1, 0.5, 1) }):Play()
		end

		local nextDisplayKey: string
		local nextTimelineStatusText: string
		local nextTimelineStatusColor: Color3
		if #HISTORY.entries == 0 then
			nextDisplayKey = "empty"
			nextTimelineStatusText = "Time Travel"
			nextTimelineStatusColor = UI.THEME_TEXT
			if ui.lastTimeTravelDisplayKey ~= nextDisplayKey then
				ui.lastTimeTravelDisplayKey = nextDisplayKey
				ui.ttSeqLabel.Text = "0 / 0"
				ui.ttSeqLabel.TextColor3 = UI.THEME_TEXT_DIM
				ui.ttLiveDot.BackgroundColor3 = UI.THEME_SURFACE_ELEVATED
				setTimeTravelTimelineVisuals(0)
			end
			elseif HISTORY.active and HISTORY.currentIndex > 0 and #HISTORY.entries > 0 then
				local ratio: number = HISTORY.currentIndex / math.max(#HISTORY.entries, 1)
				local backCount = math.max(#HISTORY.entries - HISTORY.currentIndex, 0)
				local currentEntry = HISTORY.entries[HISTORY.currentIndex]
				local geometryMarker = if currentEntry and currentEntry.geometry_affecting then "*" else ""
				nextDisplayKey = string.format("tt:%d:%d", HISTORY.currentIndex, #HISTORY.entries)
				nextTimelineStatusText = "Time Travel"
				nextTimelineStatusColor = UI.THEME_TEXT
				if ui.lastTimeTravelDisplayKey ~= nextDisplayKey then
					ui.lastTimeTravelDisplayKey = nextDisplayKey
					ui.ttSeqLabel.Text = string.format("%d/%d%s", HISTORY.currentIndex, #HISTORY.entries, geometryMarker)
					ui.ttSeqLabel.TextColor3 = UI.THEME_ACCENT
					ui.ttLiveDot.BackgroundColor3 = UI.THEME_ACCENT
					setTimeTravelTimelineVisuals(ratio)
				end
		else
			local latestEntry: HistoryEntry? = HISTORY.entries[#HISTORY.entries]
			nextDisplayKey = string.format(
				"live:%d:%s",
				#HISTORY.entries,
				if latestEntry and type(latestEntry.timestamp) == "string" then latestEntry.timestamp else "none"
			)
			nextTimelineStatusText = "Time Travel"
			nextTimelineStatusColor = UI.THEME_TEXT
			if ui.lastTimeTravelDisplayKey ~= nextDisplayKey then
				ui.lastTimeTravelDisplayKey = nextDisplayKey
				ui.ttSeqLabel.Text = string.format("LIVE · %d", #HISTORY.entries)
				ui.ttSeqLabel.TextColor3 = UI.THEME_GREEN
				ui.ttLiveDot.BackgroundColor3 = UI.THEME_GREEN
				setTimeTravelTimelineVisuals(1)
			end
		end

		if ui.lastTimelineStatusText ~= nextTimelineStatusText then
			ui.lastTimelineStatusText = nextTimelineStatusText
			ui.ttStatusLabel.Text = nextTimelineStatusText
		end
		if ui.lastTimelineStatusColor ~= nextTimelineStatusColor then
			ui.lastTimelineStatusColor = nextTimelineStatusColor
			ui.ttStatusLabel.TextColor3 = nextTimelineStatusColor
		end

		-- Show retry button if fetch failed
		if ui.lastRetryHistoryVisible ~= HISTORY.fetchFailed then
			ui.lastRetryHistoryVisible = HISTORY.fetchFailed
			ui.retryHistoryBtn.Visible = HISTORY.fetchFailed
		end

		-- Update history rows
		local entryCount: number = #HISTORY.entries
		local historyRowHeight: number = 23
		local historyBottomPadding: number = TIME_TRAVEL_LIST_BOTTOM_PADDING or 0
		local canvasHeight: number = math.max((entryCount * historyRowHeight) + historyBottomPadding, 120)
		ui.historyListFrame.CanvasSize = UDim2.new(0, 0, 0, canvasHeight)
		local scrollOffsetRows: number = math.max(math.floor(ui.historyListFrame.CanvasPosition.Y / historyRowHeight), 0)
		local historyRowCount: number = ui.historyRowCount
			or (if type(ui.historyRowFrames) == "table" then #ui.historyRowFrames else 0)
		for i = 1, historyRowCount do
			local visualIndex: number = scrollOffsetRows + i
			local rowIdx: number = entryCount - scrollOffsetRows - (i - 1)
			local rowFrame: Frame = ui.historyRowFrames[i]
			rowFrame.Position = UDim2.new(0, 0, 0, (visualIndex - 1) * historyRowHeight)
			rowFrame:SetAttribute("HistoryVisualIndex", visualIndex)
			if rowIdx >= 1 and rowIdx <= entryCount then
				local entry: HistoryEntry = HISTORY.entries[rowIdx]
				local timeStrBase: string = if type(entry.timestamp) == "string" then string.sub(entry.timestamp, 12, 19) else "??:??:??"
				local timeStr: string = if entry.geometry_affecting then ("* " .. timeStrBase) else timeStrBase
				local addedText = formatCompactMetricCount(entry.added)
				local modifiedText = formatCompactMetricCount(entry.modified)
				local deletedText = formatCompactMetricCount(entry.deleted)
				local nextRowText = string.format("%s|%s|%s|%s", timeStr, addedText, modifiedText, deletedText)
				local nextRowColor = if rowIdx == HISTORY.currentIndex then UI.THEME_ACCENT else UI.THEME_TEXT_DIM
				rowFrame:SetAttribute("HistoryEntryIndex", rowIdx)
				local isSelected = rowIdx == HISTORY.currentIndex and HISTORY.currentIndex > 0
				rowFrame.BackgroundColor3 = if isSelected then UI.THEME_SURFACE_ELEVATED else (if visualIndex % 2 == 1 then UI.THEME_SURFACE else UI.THEME_BG)
				rowFrame.BackgroundTransparency = if isSelected then 0.15 else (if visualIndex % 2 == 1 then 0.6 else 1)
				if ui.lastHistoryRowTexts[i] ~= nextRowText then
					ui.lastHistoryRowTexts[i] = nextRowText
					ui.historyRowTimeLabels[i].Text = timeStr
					ui.historyRowAddedLabels[i].Text = addedText
					ui.historyRowModifiedLabels[i].Text = modifiedText
					ui.historyRowDeletedLabels[i].Text = deletedText
				end
				if ui.lastHistoryRowColors[i] ~= nextRowColor then
					ui.lastHistoryRowColors[i] = nextRowColor
					ui.historyRowTimeLabels[i].TextColor3 = nextRowColor
					ui.historyRowAddedLabels[i].TextColor3 = nextRowColor
					ui.historyRowModifiedLabels[i].TextColor3 = nextRowColor
					ui.historyRowDeletedLabels[i].TextColor3 = nextRowColor
				end
			else
				local emptyText = if i == 1 and entryCount == 0 then "No states yet" else ""
				rowFrame:SetAttribute("HistoryEntryIndex", nil)
				rowFrame.BackgroundColor3 = if visualIndex % 2 == 1 then UI.THEME_SURFACE else UI.THEME_BG
				rowFrame.BackgroundTransparency = if visualIndex % 2 == 1 then 0.6 else 1
				if ui.lastHistoryRowTexts[i] ~= emptyText then
					ui.lastHistoryRowTexts[i] = emptyText
					ui.historyRowTimeLabels[i].Text = emptyText
					ui.historyRowAddedLabels[i].Text = ""
					ui.historyRowModifiedLabels[i].Text = ""
					ui.historyRowDeletedLabels[i].Text = ""
				end
				if ui.lastHistoryRowColors[i] ~= UI.THEME_TEXT_DIM then
					ui.lastHistoryRowColors[i] = UI.THEME_TEXT_DIM
					ui.historyRowTimeLabels[i].TextColor3 = UI.THEME_TEXT_DIM
					ui.historyRowAddedLabels[i].TextColor3 = UI.THEME_TEXT_DIM
					ui.historyRowModifiedLabels[i].TextColor3 = UI.THEME_TEXT_DIM
					ui.historyRowDeletedLabels[i].TextColor3 = UI.THEME_TEXT_DIM
				end
			end
		end
	else
		ui.lastTimeTravelDisplayKey = ""
		ui.lastTimelineStatusText = ""
	end
end


-- ─── Toolbar UI ─────────────────────────────────────────────────────────────

function Runtime.resolveToolbarIconAsset(): string
	local workspaceIcon = Workspace:GetAttribute("VertigoSyncToolbarIconAssetId")
	if type(workspaceIcon) == "string" and workspaceIcon ~= "" then
		return workspaceIcon
	end

	local savedIcon: any = plugin:GetSetting(CORE.TOOLBAR_ICON_ASSET_SETTING)
	if type(savedIcon) == "string" and savedIcon ~= "" then
		return savedIcon
	end

	return CORE.DEFAULT_TOOLBAR_ICON_ASSET
end

local toolbarIcon = Runtime.resolveToolbarIconAsset()
local toolbar = plugin:CreateToolbar("VERTIGO SYNC")
local syncButton = toolbar:CreateButton(
	"Toggle Sync",
	"Toggle Vertigo Sync realtime synchronization",
	toolbarIcon
)
local resyncButton = toolbar:CreateButton(
	"Resync",
	"Force full snapshot reconciliation",
	toolbarIcon
)
local widgetToggleButton = toolbar:CreateButton(
	"Panel",
	"Toggle Vertigo Sync panel",
	toolbarIcon
)

function Runtime.updateButtonAppearance()
	if syncEnabled and currentStatus == "connected" then
		syncButton:SetActive(true)
	else
		syncButton:SetActive(false)
	end
end

syncButton.Click:Connect(function()
	syncEnabled = not syncEnabled
	if syncEnabled then
		info("Sync enabled by user")
		resyncRequested = true
		nextPollAt = 0
		setStatusAttributes("disconnected", lastHash)
	else
		info("Sync disabled by user")
		Runtime.closeWebSocket("disabled")
		setStatusAttributes("disconnected", lastHash)
	end
	Runtime.updateButtonAppearance()
	flushMetrics(true)
end)

resyncButton.Click:Connect(function()
	if not syncEnabled then
		return
	end
	resyncRequested = true
	info("Manual resync requested")
end)

widgetToggleButton.Click:Connect(function()
	UI.widget.Enabled = not UI.widget.Enabled
	-- If opening the panel while disconnected and not first-time, show a helpful toast
	if UI.widget.Enabled and currentStatus ~= "connected" and hasEverConnected then
		showToast("Server not running — start with: vsync serve --turbo", TOAST_COLOR_INFO)
	end
end)

-- ─── Runtime loops ──────────────────────────────────────────────────────────

@native
function Runtime.tickSyncManager()
	if not syncEnabled then
		transportMode = "idle"
		Runtime.closeWebSocket("disabled")
		setStatusAttributes("disconnected", lastHash)
		Runtime.updateButtonAppearance()
		return
	end

	if not isStudioSyncMode() then
		transportMode = "idle"
		Runtime.closeWebSocket("not_studio_sync_mode")
		setStatusAttributes("disconnected", lastHash)
		Runtime.updateButtonAppearance()
		return
	end

	local now = os.clock()
	if not PROJECT.loaded or resyncRequested or lastHash == nil then
		if not ensureProjectBootstrap(false) then
			Runtime.closeWebSocket("project_bootstrap_pending")
			setStatusAttributes(if PROJECT.blocked then "error" else "disconnected", lastHash)
			Runtime.updateButtonAppearance()
			return
		end
	end

	if now - lastHealthCheckAt >= CORE.HEALTH_POLL_SECONDS then
		lastHealthCheckAt = now
		if not Runtime.checkHealth() then
			Runtime.closeWebSocket("health_failed")
			setStatusAttributes("disconnected", lastHash)
			Runtime.updateButtonAppearance()
			return
		end
	end

	if resyncRequested or lastHash == nil then
		local synced = Runtime.syncFromSnapshot(resyncRequested and "requested" or "bootstrap")
		Runtime.updateButtonAppearance()
		if not synced then
			return
		end
	end

	local wsReady = Runtime.tryConnectWebSocket()
	if wsReady and wsConnected then
		transportMode = "ws"
		Runtime.updateButtonAppearance()
		return
	end

	transportMode = "poll"
	if now >= nextPollAt then
		Runtime.pollDiff()
		Runtime.updateButtonAppearance()
		nextPollAt = now + pollInterval
		pollInterval = math.min(CORE.POLL_INTERVAL_MAX, pollInterval * 1.15)
	end
end

-- ─── Initialization ──────────────────────────────────────────────────────────

BUILDERS.enabled = SETTINGS.buildersEnabled and isEditMode()
Runtime.initInstancePool()

Workspace:SetAttribute("VertigoSyncPluginVersion", CORE.PLUGIN_VERSION)
Workspace:SetAttribute("VertigoSyncRealtimeDefault", true)
Workspace:SetAttribute("VertigoSyncBinaryModels", SETTINGS.binaryModels)
Workspace:SetAttribute("VertigoSyncWebSocketAvailable", WebSocketService ~= nil)
Workspace:SetAttribute("VertigoSyncBuildersEnabled", BUILDERS.enabled)
Workspace:SetAttribute("VertigoSyncEditPreviewEnabled", false)
Workspace:SetAttribute("VertigoPreviewBuildInProgress", false)
updateBuilderPerfAttributes()
setProjectStatus("bootstrapping", "Waiting for /project", nil, false)
setStatusAttributes("disconnected", nil)
bootstrapManagedIndex()
attachActivePathGuards()
info(string.format(
	"Plugin initialized. version=%s mode=%s ws=%s binaryModels=%s builders=%s",
	CORE.PLUGIN_VERSION,
	describeStudioMode(),
	if WebSocketService ~= nil then "available" else "unavailable",
	tostring(SETTINGS.binaryModels),
	tostring(BUILDERS.enabled)
))
Runtime.updateButtonAppearance()
flushMetrics(true)

task.defer(function()
	ensureProjectBootstrap(false)
end)

Workspace:GetAttributeChangedSignal("VertigoSyncEditPreviewSuspended"):Connect(function()
	if Workspace:GetAttribute("VertigoSyncEditPreviewSuspended") == true then
		Runtime.cancelPendingEditPreviewBuild("workspace_suspended")
		Runtime.clearEditPreviewWatchers()
	else
		PROJECT.editPreview.nextRootRefreshAt = 0.0
		PROJECT.editPreview.initialBuildQueued = false
	end
end)

Workspace:GetAttributeChangedSignal("VertigoSyncRunAllActive"):Connect(function()
	if Workspace:GetAttribute("VertigoSyncRunAllActive") == true then
		Runtime.cancelPendingEditPreviewBuild("runall_suite_active")
	else
		PROJECT.editPreview.nextRootRefreshAt = 0.0
		PROJECT.editPreview.initialBuildQueued = false
	end
end)

Workspace:GetAttributeChangedSignal("VertigoPreviewForceRebuild"):Connect(function()
	if Workspace:GetAttribute("VertigoPreviewForceRebuild") == true then
		Workspace:SetAttribute("VertigoPreviewForceRebuild", false)
		Runtime.scheduleEditPreviewBuild("workspace_attribute")
	end
end)

-- ─── Heartbeat Loop ──────────────────────────────────────────────────────────

	RunService.Heartbeat:Connect(function()
		-- Keep fetch/apply running even during historical mode so rewound snapshots fully materialize.
		-- Polling/WS remain gated in Runtime.tickSyncManager().
		Runtime.processFetchQueue()
		Runtime.processApplyQueue()
		if HISTORY.needsBuilderReconcile then
			local pendingDepth: number = math.max(#pendingQueue - pendingQueueHead + 1, 0)
			local fetchDepth: number = math.max(#fetchQueue - fetchQueueHead + 1, 0)
			if pendingDepth == 0 and fetchDepth == 0 and fetchInFlight == 0 and not BUILDERS.pumpActive then
				HISTORY.needsBuilderReconcile = false
				BUILDERS.scheduleFullReconcile()
			end
		end
		flushMetrics(false)
	end)

-- ─── Sync Manager Loop ──────────────────────────────────────────────────────

task.spawn(function()
	while true do
		Runtime.tickEditPreview()
		if not HISTORY.active then
			Runtime.tickSyncManager()
		end
		task.wait(0.25)
	end
end)

-- ─── UI Refresh Loop (0.5s timer, NOT Heartbeat) ────────────────────────────

task.spawn(function()
	while true do
		if UI.widget.Enabled then
			Runtime.refreshStatusUI()
			task.wait(FEATURES.UI_STATUS_REFRESH_SECONDS)
		else
			task.wait(FEATURES.UI_STATUS_REFRESH_HIDDEN_SECONDS)
		end
	end
end)

if UI.historyListFrame ~= nil then
	UI.historyListFrame:GetPropertyChangedSignal("CanvasPosition"):Connect(function()
		if UI.widget.Enabled and SETTINGS.timeTravelUI then
			Runtime.refreshStatusUI()
		end
	end)
end

-- ─── History Background Fetch ────────────────────────────────────────────────

task.spawn(function()
	-- Wait for initial sync to complete before fetching history
	task.wait(3)
	while true do
		if UI.widget.Enabled and SETTINGS.timeTravelUI and currentStatus == "connected" and not HISTORY.active then
			TimeTravel.fetchHistory()
		end
		task.wait(FEATURES.HISTORY_REFRESH_INTERVAL_SECONDS)
	end
end)

-- ─── Builder Initial Execution (after first sync) ───────────────────────────

task.spawn(function()
	-- Wait for initial snapshot to complete
	task.wait(5)
	if BUILDERS.enabled and currentStatus == "connected" then
		runInitialBuilders()
	end
end)

-- ─── State Reporting Loop (3s timer, independent of sync) ────────────────────

task.spawn(function()
	task.wait(STATE_REPORT_INTERVAL_SECONDS)
	while true do
		Runtime.reportPluginState()
		task.wait(STATE_REPORT_INTERVAL_SECONDS)
	end
end)

-- ─── Managed Index Reporting Loop (30s timer) ────────────────────────────────

task.spawn(function()
	-- Wait for initial sync to settle
	task.wait(MANAGED_REPORT_INTERVAL_SECONDS)
	while true do
		if currentStatus == "connected" then
			Runtime.reportPluginManaged()
		end
		task.wait(MANAGED_REPORT_INTERVAL_SECONDS)
	end
end)


end -- UI._initPlugin
UI._initPlugin()
Workspace:GetAttributeChangedSignal("VertigoSyncServerUrl"):Connect(handleServerUrlChanged)
