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
	  GET /snapshot
	  GET /diff?since=<fingerprint>
	  GET /source/{path}
	  GET /sources
	  GET /sources/content?paths=<csv>
	  GET /events
	  GET /ws
	  GET /history?limit=N
	  GET /rewind?to=<fingerprint>
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

local LOG_PREFIX = "[VertigoSync]"
local PLUGIN_VERSION = "2026-03-16-v9-trillion-dollar"
-- PLUGIN_SEMVER "0.1.0" used inline in status line 3 (no new local to stay under 194 register limit)

local DEFAULT_SERVER_BASE_URL = "http://127.0.0.1:7575"
local HEALTH_POLL_SECONDS = 15

local POLL_INTERVAL_FAST = 0.10
local POLL_INTERVAL_MAX = 1.50

local APPLY_FRAME_BUDGET_SECONDS = 0.002
local MAX_APPLIES_PER_TICK = 16
local MAX_FETCH_CONCURRENCY = 8
local MAX_SOURCE_FETCH_RETRIES = 3
local MAX_SOURCE_BATCH_SIZE = 8
local APPLY_FRAME_BUDGET_MIN_SECONDS = APPLY_FRAME_BUDGET_SECONDS * 0.75
local APPLY_FRAME_BUDGET_MAX_SECONDS = APPLY_FRAME_BUDGET_SECONDS * 3.0
local APPLY_MIN_APPLIES_PER_TICK = math.max(4, math.floor(MAX_APPLIES_PER_TICK * 0.5))
local APPLY_MAX_APPLIES_HARD_LIMIT = MAX_APPLIES_PER_TICK * 4
local APPLY_QUEUE_HIGH_WATERMARK = 512
local APPLY_QUEUE_HARD_CAP = 512
local APPLY_BUDGET_EWMA_ALPHA = 0.22
local APPLY_BUDGET_RECALC_SECONDS = 0.25
local FETCH_CONCURRENCY_MIN = math.max(8, math.floor(MAX_FETCH_CONCURRENCY * 0.5))
local FETCH_CONCURRENCY_MAX = MAX_FETCH_CONCURRENCY * 3

local WS_RECONNECT_MIN_SECONDS = 0.25
local WS_RECONNECT_MAX_SECONDS = 5.0

local METRIC_FLUSH_SECONDS = 2.0
local SELF_MUTATION_GUARD_SECONDS = 1.75

local MANAGED_PATH_ATTR = "VertigoSyncPath"
local MANAGED_SHA_ATTR = "VertigoSyncSha256"

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

local BINARY_MODELS_ENABLED = false
local BUILDERS_ENABLED_DEFAULT = true -- Enabled: incremental rebuilds on file change. Initial full-build skipped if .rbxl has baked geometry.
local TIME_TRAVEL_HISTORY_LIMIT = 256
local UI_STATUS_REFRESH_SECONDS = 0.5
local HISTORY_REFRESH_INTERVAL_SECONDS = 5

-- ─── Instance Pool Constants ─────────────────────────────────────────────────

local POOL_SIZE = 128
local POOLED_CLASSES = table.freeze({
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
})

-- ─── Builder Constants ───────────────────────────────────────────────────────

local BUILDER_PATHS = table.freeze({
	"src/Server/World/Builders/",
})

local BUILDER_DEPENDENCY_PATHS: { string } = table.freeze({
	"src/Server/World/Elements/",
	"src/Shared/Config/",
	"src/Shared/Util/",
})

local BUILDER_DEBOUNCE_SECONDS = 0.25

-- ─── Types ───────────────────────────────────────────────────────────────────

type SyncStatus = "connected" | "disconnected" | "error"
type TransportMode = "idle" | "ws" | "poll"
type ConnectionState = "waiting" | "connecting" | "connected" | "reconnecting" | "error"
type PendingAction = "write" | "delete" | "model_apply"
type ProjectBootstrapMode = "bootstrapping" | "dynamic" | "legacy" | "mismatch"

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
	mappings: { ProjectMappingEntry },
	emit_legacy_scripts: boolean?,
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

-- ─── Path Mapping ────────────────────────────────────────────────────────────

local LEGACY_PATH_MAPPINGS: { PathMapping } = table.freeze({
	table.freeze({
		prefix = "src/Server/",
		root = ServerScriptService,
		instancePath = "ServerScriptService.Server",
		containerSegments = table.freeze({}),
		boundaryName = "Server",
	}),
	table.freeze({
		prefix = "src/Client/",
		root = StarterPlayer,
		instancePath = "StarterPlayer.StarterPlayerScripts.Client",
		containerSegments = table.freeze({ "StarterPlayerScripts" }),
		boundaryName = "Client",
	}),
	table.freeze({
		prefix = "src/Shared/",
		root = ReplicatedStorage,
		instancePath = "ReplicatedStorage.Shared",
		containerSegments = table.freeze({}),
		boundaryName = "Shared",
	}),
	table.freeze({
		prefix = "Packages/",
		root = ReplicatedStorage,
		instancePath = "ReplicatedStorage.Packages",
		containerSegments = table.freeze({}),
		boundaryName = "Packages",
	}),
}) :: { PathMapping }

local LEGACY_PATH_PREFIX_LENS: { number } = table.freeze({
	#(LEGACY_PATH_MAPPINGS[1] :: PathMapping).prefix,
	#(LEGACY_PATH_MAPPINGS[2] :: PathMapping).prefix,
	#(LEGACY_PATH_MAPPINGS[3] :: PathMapping).prefix,
	#(LEGACY_PATH_MAPPINGS[4] :: PathMapping).prefix,
}) :: { number }

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
local pollInterval = POLL_INTERVAL_FAST

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
local wsReconnectBackoffSeconds = WS_RECONNECT_MIN_SECONDS
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

local applyWindowStart = os.clock()
local appliedInWindow = 0
local appliedPerSecond = 0
local lastMetricFlushAt = 0.0
local adaptiveApplyBudgetSeconds = APPLY_FRAME_BUDGET_SECONDS
local adaptiveMaxAppliesPerTick = MAX_APPLIES_PER_TICK
local adaptiveFetchConcurrency = MAX_FETCH_CONCURRENCY
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
}

-- ─── Builder State ───────────────────────────────────────────────────────────

local BUILDERS = {
	enabled = false, -- set in init based on edit mode
	sources = {} :: { [string]: string }, -- path -> source hash
	outputTags = {} :: { [string]: string }, -- path -> output tag
	dependencyMap = {} :: { [string]: { [string]: boolean } }, -- shared module path -> set of builder paths
	dirtySet = {} :: { [string]: boolean }, -- builder paths pending re-execution
	debounceScheduled = false,
}

-- ─── Settings State ──────────────────────────────────────────────────────────

local SETTINGS = {
	binaryModels = BINARY_MODELS_ENABLED,
	buildersEnabled = BUILDERS_ENABLED_DEFAULT,
	timeTravelUI = true,
	historyBuffer = TIME_TRAVEL_HISTORY_LIMIT,
}

-- Log level: 0=quiet, 1=normal, 2=verbose (controllable via sync_plugin_command)
local logLevel: number = 1

-- ─── Plugin Boot Tracking ────────────────────────────────────────────────────

local pluginBootTime: number = os.clock()
local serverBootTimeCache: number? = nil -- cached server_boot_time from /health

-- ─── Project Bootstrap State ────────────────────────────────────────────────

local PROJECT = {
	mappings = {} :: { PathMapping },
	prefixLens = {} :: { number },
	mode = "bootstrapping" :: ProjectBootstrapMode,
	message = "Waiting for /project",
	name = nil :: string?,
	mappingCount = 0,
	loaded = false,
	blocked = false,
	emitLegacyScripts = true,
	lastStatusToastKey = "",
	attachedRootGuards = {} :: { [Instance]: boolean },
}
local resolveMapping: (filePath: string) -> (PathMapping?, string?)
local bootstrapManagedIndex: () -> ()

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
	print(string.format("%s %s", LOG_PREFIX, message))
end

local function warnMsg(message: string)
	warn(string.format("%s %s", LOG_PREFIX, message))
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
		warn(LOG_PREFIX .. " " .. message)
	else
		print(LOG_PREFIX .. " " .. message)
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
	local guardUntil: number = os.clock() + SELF_MUTATION_GUARD_SECONDS
	if guardUntil > selfMutationGuardUntil then
		selfMutationGuardUntil = guardUntil
	end
end

@native
local function inSelfMutationGuard(): boolean
	return os.clock() < selfMutationGuardUntil
end

@native
local function isEditMode(): boolean
	return RunService:IsEdit() and not RunService:IsRunning()
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

local function setStatusAttributes(status: SyncStatus, hash: string?)
	currentStatus = status
	Workspace:SetAttribute("VertigoSyncStatus", status)
	if hash then
		Workspace:SetAttribute("VertigoSyncHash", hash)
	end
	Workspace:SetAttribute("VertigoSyncLastUpdate", os.date("!%Y-%m-%dT%H:%M:%SZ"))
end

local function setProjectStatus(mode: ProjectBootstrapMode, message: string, projectName: string?, blocked: boolean)
	PROJECT.mode = mode
	PROJECT.message = message
	PROJECT.name = projectName
	PROJECT.blocked = blocked

	Workspace:SetAttribute("VertigoSyncProjectMode", mode)
	Workspace:SetAttribute("VertigoSyncProjectName", projectName)
	Workspace:SetAttribute("VertigoSyncProjectMessage", message)
	Workspace:SetAttribute("VertigoSyncProjectBlocked", blocked)
	Workspace:SetAttribute("VertigoSyncProjectLegacy", mode == "legacy")
	Workspace:SetAttribute("VertigoSyncProjectMismatch", mode == "mismatch")
	Workspace:SetAttribute("VertigoSyncProjectMappingCount", PROJECT.mappingCount)
	Workspace:SetAttribute("VertigoSyncEmitLegacyScripts", PROJECT.emitLegacyScripts)

	if (mode == "legacy" or mode == "mismatch") and message ~= "" then
		local toastKey = mode .. "::" .. message
		if toastKey ~= PROJECT.lastStatusToastKey then
			PROJECT.lastStatusToastKey = toastKey
			showToast(message, if mode == "legacy" then TOAST_COLOR_INFO else TOAST_COLOR_ERROR)
		end
	end
end

@native
-- METRIC_DEBUG_VERBOSE: set to true below to emit all diagnostic attributes
local function flushMetrics(force: boolean)
	local now = os.clock()
	if not force and now - lastMetricFlushAt < METRIC_FLUSH_SECONDS then
		return
	end
	lastMetricFlushAt = now

	-- Essential metrics (always emitted)
	Workspace:SetAttribute("VertigoSyncQueueDepth", math.max(#pendingQueue - pendingQueueHead + 1, 0))
	Workspace:SetAttribute("VertigoSyncPluginVersion", PLUGIN_VERSION)

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
		Workspace:SetAttribute("VertigoSyncTimeTravel", HISTORY.active)
	end
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

local function getServerBaseUrl(): string
	local rawValue: any = Workspace:GetAttribute("VertigoSyncServerUrl")
	if type(rawValue) ~= "string" or rawValue == "" then
		rawValue = plugin:GetSetting("VertigoSyncServerUrl")
	end
	if type(rawValue) ~= "string" or rawValue == "" then
		return DEFAULT_SERVER_BASE_URL
	end
	local trimmed = string.gsub(rawValue, "%s+", "")
	trimmed = string.gsub(trimmed, "/+$", "")
	if trimmed == "" then
		return DEFAULT_SERVER_BASE_URL
	end
	if string.sub(trimmed, 1, 7) ~= "http://" and string.sub(trimmed, 1, 8) ~= "https://" then
		return DEFAULT_SERVER_BASE_URL
	end
	return trimmed
end

local function requestRaw(endpoint: string): (boolean, any)
	local url = getServerBaseUrl() .. endpoint
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

local function requestJson(endpoint: string): (boolean, any, number)
	local ok, rawOrErr = requestRaw(endpoint)
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

local function projectStatusLabel(): string
	if PROJECT.mode == "legacy" then
		return "legacy"
	end
	if PROJECT.mode == "mismatch" then
		if PROJECT.name ~= nil and PROJECT.name ~= "" then
			return "mismatch:" .. string.sub(PROJECT.name, 1, 16)
		end
		return if PROJECT.blocked then "mismatch:block" else "mismatch"
	end
	if PROJECT.name ~= nil and PROJECT.name ~= "" then
		return string.sub(PROJECT.name, 1, 18)
	end
	return if PROJECT.mode == "bootstrapping" then "waiting" else "dynamic"
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
		local managedPath = descendant:GetAttribute(MANAGED_PATH_ATTR)
		if type(managedPath) == "string" and managedPath ~= "" and resolveMapping(managedPath) ~= nil then
			managedIndex[managedPath] = descendant
			local shaAttr: any = descendant:GetAttribute(MANAGED_SHA_ATTR)
			if type(shaAttr) == "string" and shaAttr ~= "" then
				managedShaByPath[managedPath] = shaAttr
			end
		end
	end)

	root.DescendantRemoving:Connect(function(descendant: Instance)
		if inSelfMutationGuard() then
			return
		end
		local managedPath = descendant:GetAttribute(MANAGED_PATH_ATTR)
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

local function applyProjectPayload(payload: any): boolean
	if type(payload) ~= "table" then
		setProjectStatus("mismatch", "Malformed /project payload", nil, true)
		return false
	end
	if type(payload.name) ~= "string" or type(payload.mappings) ~= "table" then
		setProjectStatus("mismatch", "Incomplete /project payload", nil, true)
		return false
	end

	local projectName: string = payload.name
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
	activateDynamicPathMappings(runtimeMappings)
	PROJECT.loaded = true
	PROJECT.blocked = false
	bootstrapManagedIndex()
	attachActivePathGuards()

	local statusMessage: string
	if skippedMappings > 0 then
		statusMessage = string.format("Loaded /project '%s' (%d mappings, %d skipped)", projectName, #runtimeMappings, skippedMappings)
	else
		statusMessage = string.format("Loaded /project '%s' (%d mappings)", projectName, #runtimeMappings)
	end

	setProjectStatus("dynamic", statusMessage, projectName, false)
	return true
end

local function ensureProjectBootstrap(force: boolean): boolean
	if PROJECT.loaded and not force then
		return not PROJECT.blocked
	end

	local ok, payloadOrErr, statusCode = requestJson("/project")
	if ok then
		return applyProjectPayload(payloadOrErr)
	end

	if statusCode == 404 then
		PROJECT.loaded = false
		PROJECT.blocked = true
		setProjectStatus("mismatch", string.format("Server at %s does not expose /project", getServerBaseUrl()), PROJECT.name, true)
		return false
	end

	if statusCode == 0 then
		if PROJECT.mode == "bootstrapping" then
			setProjectStatus("bootstrapping", "Waiting for /project", PROJECT.name, false)
		end
		return false
	end

	PROJECT.loaded = false
	PROJECT.blocked = true
	setProjectStatus("mismatch", string.format("Failed to load /project: %s", tostring(payloadOrErr)), PROJECT.name, true)
	return false
end

local function handleServerUrlChanged()
	PROJECT.loaded = false
	PROJECT.blocked = false
	resyncRequested = true
	closeWebSocket("server_url_changed")
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
						local fetchOk: boolean, fetchErr: string? = pcall(fetchHistory)
						if not fetchOk then
							success = false
							message = "fetchHistory threw: " .. tostring(fetchErr)
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
							local rwOk: boolean, rwErr: string? = pcall(rewindToIndex, targetIndex)
							if not rwOk then
								success = false
								message = "rewindToIndex threw: " .. tostring(rwErr)
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
					local fetchOk: boolean = pcall(fetchHistory)
					if not fetchOk or not HISTORY.loaded then
						success = false
						message = "failed to load history for step_back"
					end
				end
				if success then
					local sbOk: boolean, sbErr: string? = pcall(stepBackward)
					if not sbOk then
						success = false
						message = "stepBackward threw: " .. tostring(sbErr)
					else
						message = "stepped back"
					end
				end
			elseif params.action == "step_forward" then
				local sfOk: boolean, sfErr: string? = pcall(stepForward)
				if not sfOk then
					success = false
					message = "stepForward threw: " .. tostring(sfErr)
				else
					message = "stepped forward"
				end
			elseif params.action == "jump_oldest" then
				if not HISTORY.loaded then
					local fetchOk: boolean = pcall(fetchHistory)
					if not fetchOk or not HISTORY.loaded then
						success = false
						message = "failed to load history for jump_oldest"
					end
				end
				if success then
					local joOk: boolean, joErr: string? = pcall(jumpToOldest)
					if not joOk then
						success = false
						message = "jumpToOldest threw: " .. tostring(joErr)
					else
						message = "jumped to oldest snapshot"
					end
				end
			elseif params.action == "resume_live" then
				local rlOk: boolean, rlErr: string? = pcall(resumeLiveSync)
				if not rlOk then
					success = false
					message = "resumeLiveSync threw: " .. tostring(rlErr)
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
end

-- ─── State Reporting (POST to server, never crashes, never logs on failure) ─

local lastStateReportAt: number = 0
local lastManagedReportAt: number = 0

local function reportPluginState()
	local now: number = os.clock()
	if now - lastStateReportAt < STATE_REPORT_INTERVAL_SECONDS then
		return
	end
	lastStateReportAt = now

	local queueDepth: number = math.max(#pendingQueue - pendingQueueHead + 1, 0)
	local fetchQueueDepth: number = math.max(#fetchQueue - fetchQueueHead + 1, 0)
	local managedCount: number = 0
	for _ in managedIndex do
		managedCount += 1
	end

	local builderLastRebuild: number? = Workspace:GetAttribute("VertigoBuilderLastRebuild") :: number?
	local builderLastPath: string? = Workspace:GetAttribute("VertigoBuilderLastRebuildPath") :: string?

	local payload: { [string]: any } = {
		plugin_version = PLUGIN_VERSION,
		status = currentStatus,
		transport = transportMode,
		hash = lastHash,
		queue_depth = queueDepth,
		fetch_queue_depth = fetchQueueDepth,
		fetch_in_flight = fetchInFlight,
		applies_per_second = appliedPerSecond,
		apply_budget_ms = math.floor(adaptiveApplyBudgetSeconds * 1000 + 0.5),
		apply_cost_us = math.floor(applyCostEwmaSeconds * 1000000 + 0.5),
		reconnects = reconnectCount,
		lagged_events = laggedEvents,
		dropped_updates = droppedUpdates,
		managed_count = managedCount,
		time_travel_active = HISTORY.active,
		time_travel_seq = if HISTORY.active then HISTORY.currentIndex else nil,
		binary_models_enabled = SETTINGS.binaryModels,
		builders_enabled = BUILDERS.enabled,
		builders_last_rebuild = builderLastRebuild,
		builders_last_path = builderLastPath,
		project_mode = PROJECT.mode,
		project_name = PROJECT.name,
		project_message = PROJECT.message,
		project_blocked = PROJECT.blocked,
		project_mapping_count = PROJECT.mappingCount,
		project_emit_legacy_scripts = PROJECT.emitLegacyScripts,
		studio_mode = describeStudioMode(),
		uptime_seconds = math.floor(now - pluginBootTime + 0.5),
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

local function reportPluginManaged()
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

local function initInstancePool()
	local classCount: number = #POOLED_CLASSES
	for i = 1, classCount do
		local className: string = POOLED_CLASSES[i]
		local pool: { Instance } = table.create(POOL_SIZE)
		for j = 1, POOL_SIZE do
			local inst: Instance = Instance.new(className)
			inst.Parent = nil
			pool[j] = inst
		end
		instancePool[className] = pool
	end
end

@native
local function poolGet(className: string): Instance
	local pool: { Instance }? = instancePool[className]
	if pool ~= nil and #pool > 0 then
		local inst: Instance = table.remove(pool) :: Instance
		return inst
	end
	return Instance.new(className)
end

@native
local function poolReturn(inst: Instance)
	local pool: { Instance }? = instancePool[inst.ClassName]
	if pool ~= nil and #pool < POOL_SIZE then
		inst.Parent = nil
		inst.Name = ""
		table.insert(pool, inst)
	else
		inst:Destroy()
	end
end

-- ─── Queue helpers ──────────────────────────────────────────────────────────

@native
local function compactPathQueueIfNeeded()
	if pendingQueueHead > 1024 and pendingQueueHead > #pendingQueue then
		pendingQueue = {}
		pendingQueueHead = 1
	end
end

@native
local function enqueuePath(path: string)
	if #pendingQueue - pendingQueueHead + 1 > APPLY_QUEUE_HARD_CAP then
		warn("[VertigoSync] Apply queue overflow — forcing resync")
		pendingQueue = {}
		pendingQueueHead = 1
		pendingOps = {}
		readySources = {}
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
local function popPendingPath(): string?
	local queueLen: number = #pendingQueue
	while pendingQueueHead <= queueLen do
		local path: string = pendingQueue[pendingQueueHead]
		pendingQueue[pendingQueueHead] = ""
		pendingQueueHead += 1
		if path ~= "" then
			compactPathQueueIfNeeded()
			return path
		end
	end
	compactPathQueueIfNeeded()
	return nil
end

@native
local function compactFetchQueueIfNeeded()
	if fetchQueueHead > 1024 and fetchQueueHead > #fetchQueue then
		fetchQueue = {}
		fetchQueueHead = 1
	end
end

@native
local function pushFetchTask(path: string, epoch: number)
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
local function popFetchTask(): FetchTask?
	local queueLen: number = #fetchQueue
	while fetchQueueHead <= queueLen do
		local taskItem: FetchTask? = fetchQueue[fetchQueueHead]
		fetchQueue[fetchQueueHead] = nil
		fetchQueueHead += 1
		if taskItem ~= nil then
			compactFetchQueueIfNeeded()
			return taskItem
		end
	end
	compactFetchQueueIfNeeded()
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
local function stripExtension(name: string): string
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
local function isInitFile(name: string): boolean
	return INIT_FILES[name] == true
end

@native
local function classForFile(fileName: string): string
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

local function runContextForPath(filePath: string): Enum.RunContext?
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
local function fileTypeForPath(filePath: string): string?
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
local function isBinaryModelType(fileType: string?): boolean
	return fileType == "rbxm" or fileType == "rbxmx"
end

@native
local function parseRelativePath(relativePath: string): ({ string }, string, string)
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
	if isInitFile(fileName) then
		local className: string = classForFile(fileName)
		segments[segCount] = nil :: any
		segCount -= 1
		if segCount == 0 then
			return {}, className, ""
		end
		local instanceName: string = segments[segCount]
		segments[segCount] = nil :: any
		return segments, className, instanceName
	end

	local className: string = classForFile(fileName)
	local instanceName: string = stripExtension(fileName)
	segments[segCount] = nil :: any
	return segments, className, instanceName
end

local function ensureContainer(parent: Instance, childName: string): Instance
	local existing = parent:FindFirstChild(childName)
	if existing ~= nil then
		return existing
	end

	refreshSelfMutationGuard()
	local folder = poolGet("Folder")
	folder.Name = childName
	folder.Parent = parent
	return folder
end

@native
local function ensureAncestors(root: Instance, segments: { string }): Instance
	local current: Instance = root
	local segCount: number = #segments
	for i = 1, segCount do
		current = ensureContainer(current, segments[i])
	end
	return current
end

-- Ensure or create an instance of the given class under parent with the given name.
-- If an existing child has the wrong class, replace it.
local function ensureOrCreate(parent: Instance, instanceName: string, className: string): Instance
	local existing: Instance? = parent:FindFirstChild(instanceName)
	if existing ~= nil then
		if existing.ClassName == className then
			return existing
		end
		-- Wrong class: replace
		refreshSelfMutationGuard()
		local replacement: Instance = poolGet(className)
		replacement.Name = instanceName
		for _, child in ipairs(existing:GetChildren()) do
			child.Parent = replacement
		end
		replacement.Parent = parent
		poolReturn(existing)
		return replacement
	end
	refreshSelfMutationGuard()
	local created: Instance = poolGet(className)
	created.Name = instanceName
	created.Parent = parent
	return created
end

@native
local function findBoundaryRoot(mapping: PathMapping): Instance?
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

local function visitManagedScope(scopeRoot: Instance, callback: (Instance) -> ())
	callback(scopeRoot)
	local descendants: { Instance } = scopeRoot:GetDescendants()
	for i = 1, #descendants do
		callback(descendants[i])
	end
end

@native
local function resolveTarget(filePath: string): (Instance?, string?, string?, Instance?)
	local mapping, remainder = resolveMapping(filePath)
	if mapping == nil or remainder == nil then
		return nil, nil, nil, nil
	end

	local segments, className, instanceName = parseRelativePath(remainder)
	local boundaryParent: Instance = mapping.root
	if #mapping.containerSegments > 0 then
		boundaryParent = ensureAncestors(mapping.root, mapping.containerSegments)
	end

	if instanceName == "" then
		if mapping.boundaryName == nil or mapping.boundaryName == "" then
			return nil, nil, nil, nil
		end
		return boundaryParent, mapping.boundaryName, className, boundaryParent
	end

	local boundary: Instance = boundaryParent
	if mapping.boundaryName ~= nil and mapping.boundaryName ~= "" then
		boundary = ensureContainer(boundaryParent, mapping.boundaryName)
	end
	local parent = ensureAncestors(boundary, segments)
	return parent, instanceName, className, boundary
end

local function replaceInstanceClassPreservingChildren(existing: Instance, className: string): Instance
	refreshSelfMutationGuard()
	local replacement = poolGet(className)
	replacement.Name = existing.Name

	for _, child in ipairs(existing:GetChildren()) do
		child.Parent = replacement
	end

	local parent = existing.Parent
	poolReturn(existing)
	replacement.Parent = parent
	return replacement
end

local function cleanupEmptyAncestors(parent: Instance?, boundary: Instance?)
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
		poolReturn(current)
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
		local boundaryRoot: Instance? = findBoundaryRoot(mapping)
		if boundaryRoot ~= nil then
			visitManagedScope(boundaryRoot, function(descendant: Instance)
				local pathAttr: any = descendant:GetAttribute(MANAGED_PATH_ATTR)
				if type(pathAttr) == "string" and pathAttr ~= "" and resolveMapping(pathAttr) ~= nil then
					managedIndex[pathAttr] = descendant
					local shaAttr: any = descendant:GetAttribute(MANAGED_SHA_ATTR)
					if type(shaAttr) == "string" and shaAttr ~= "" then
						managedShaByPath[pathAttr] = shaAttr
					end
				end
			end)
		end
	end
end

-- ─── Meta property application ──────────────────────────────────────────────

local function applyMeta(inst: Instance, meta: EntryMeta?)
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
-- Uses MANAGED_PATH_ATTR to find root instances, then destroys the entire subtree.
local function cleanupModelInstances(path: string)
	-- Find all root instances tagged with this model path
	local mappingCount: number = #PROJECT.mappings
	for i = 1, mappingCount do
		local mapping: PathMapping = PROJECT.mappings[i]
		local boundaryRoot: Instance? = findBoundaryRoot(mapping)
		if boundaryRoot ~= nil then
			local pendingDestroy: { Instance } = {}
			visitManagedScope(boundaryRoot, function(descendant: Instance)
				local pathAttr: any = descendant:GetAttribute(MANAGED_PATH_ATTR)
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
local function stageModelManifest(path: string, manifest: ModelManifest, epoch: number, sha256: string?)
	-- Clean up any existing model instances at this path
	cleanupModelInstances(path)

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
	enqueuePath(path)

	info(string.format("Staged model manifest: %s (%d instances, %d roots)", path, instanceCount, manifest.rootCount))
end

-- Process one model instance creation op from the staged queue.
-- Returns true if an instance was created, false if the queue is exhausted.
@native
local function processOneModelOp(path: string): boolean
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
	local inst: Instance = poolGet(entry.className)
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
		local parent, _instanceName, _className, _boundary = resolveTarget(path)
		if parent ~= nil then
			refreshSelfMutationGuard()
			inst.Parent = parent
			-- Tag root instances for managed tracking
			inst:SetAttribute(MANAGED_PATH_ATTR, path)
			if managedShaByPath[path] ~= nil then
				inst:SetAttribute(MANAGED_SHA_ATTR, managedShaByPath[path])
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

-- Require-cache invalidation: clone the ModuleScript, require the clone, destroy
-- the clone. This forces Luau to re-evaluate the source instead of returning the
-- cached result from a previous require() call.
local function requireFresh(scriptInstance: ModuleScript): (boolean, any)
	local clone: ModuleScript = scriptInstance:Clone()
	clone.Name = scriptInstance.Name .. "_BuilderClone"
	clone.Parent = scriptInstance.Parent
	local ok: boolean, result: any = pcall(require, clone)
	clone:Destroy()
	return ok, result
end

-- Scan builder source for require() calls and populate the dependency map.
-- Uses string.find (plain) to locate `require(` tokens, then extracts the
-- path segments to map shared module paths back to builder paths.
local function computeBuilderDependencies(builderPath: string, scriptInstance: LuaSourceContainer)
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
		local depPathCount: number = #BUILDER_DEPENDENCY_PATHS
		for i = 1, depPathCount do
			local depPrefix: string = BUILDER_DEPENDENCY_PATHS[i]
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

-- Schedule a debounced batch of dirty builder re-executions
local function scheduleBuilderBatch()
	if BUILDERS.debounceScheduled then
		return
	end
	BUILDERS.debounceScheduled = true
	task.defer(function()
		task.wait(BUILDER_DEBOUNCE_SECONDS)
		BUILDERS.debounceScheduled = false

		local dirtyPaths: { string } = {}
		for path: string in BUILDERS.dirtySet do
			table.insert(dirtyPaths, path)
		end
		BUILDERS.dirtySet = {}

		if #dirtyPaths == 0 then
			return
		end

		-- Sort for deterministic order, HubBuilder first (spawn zone)
		table.sort(dirtyPaths, function(a: string, b: string): boolean
			local aIsHub: boolean = string.find(a, "HubBuilder", 1, true) ~= nil
			local bIsHub: boolean = string.find(b, "HubBuilder", 1, true) ~= nil
			if aIsHub ~= bIsHub then
				return aIsHub
			end
			return a < b
		end)

		for _, path: string in dirtyPaths do
			local inst: Instance? = managedIndex[path]
			if inst ~= nil and inst:IsA("ModuleScript") then
				-- Re-execute inline (already deferred via the batch wait)
				local sourceHash: string = inst:GetAttribute(MANAGED_SHA_ATTR) or "unknown"
				local segments: { string } = string.split(path, "/")
				local joined: string = table.concat(segments, "_")
				local parts: { string } = string.split(joined, ".")
				local sanitized: string = table.concat(parts, "_")
				local outputTag: string = "BuilderOutput_" .. sanitized .. "_" .. string.sub(sourceHash, 1, 8)

				-- Check if output already exists with matching hash
				if BUILDERS.outputTags[path] == outputTag then
					local existingOutputs: { Instance } = CollectionService:GetTagged(outputTag)
					if #existingOutputs > 0 then
						continue
					end
				end

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

				-- Execute with fresh require to bypass cache
				local execOk: boolean, result: any = requireFresh(inst :: ModuleScript)

				if execOk and type(result) == "table" then
					if type(result.Init) == "function" then
						pcall(result.Init, result)
					end
					if type(result.Build) == "function" then
						pcall(result.Build, result)
					end
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

				-- Tag all captured output and persist build hash for cross-session stale detection
				for _, child: Instance in newChildren do
					CollectionService:AddTag(child, outputTag)
					child:SetAttribute("VertigoBuildHash", sourceHash)
					child:SetAttribute("VertigoBuildPath", path)
				end

				-- Recompute dependencies for the builder
				computeBuilderDependencies(path, inst :: LuaSourceContainer)

				BUILDERS.sources[path] = sourceHash
				BUILDERS.outputTags[path] = outputTag

				-- Signal runtime to refresh tag caches
				Workspace:SetAttribute("VertigoBuilderLastRebuild", os.clock())
				Workspace:SetAttribute("VertigoBuilderLastRebuildPath", path)

				info(string.format("Builder executed: %s (hash=%s, captured=%d)", path, string.sub(sourceHash, 1, 8), #newChildren))
			end
		end
	end)
end

local function executeBuilder(path: string, scriptInstance: LuaSourceContainer)
	local sourceHash: string = scriptInstance:GetAttribute(MANAGED_SHA_ATTR) or "unknown"
	-- Use string.split instead of gsub for NCG compliance
	local segments: { string } = string.split(path, "/")
	local joined: string = table.concat(segments, "_")
	local parts: { string } = string.split(joined, ".")
	local sanitized: string = table.concat(parts, "_")
	local outputTag: string = "BuilderOutput_" .. sanitized .. "_" .. string.sub(sourceHash, 1, 8)

	-- Check if output already exists with matching hash
	local existingOutputs: { Instance } = CollectionService:GetTagged(BUILDERS.outputTags[path] or "")
	if #existingOutputs > 0 and BUILDERS.outputTags[path] == outputTag then
		return -- Builder output is current, skip
	end

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
	local execOk: boolean, result: any = requireFresh(scriptInstance :: ModuleScript)

	if execOk and type(result) == "table" then
		-- If the builder returns a table with :Init() and :Build(), call them
		if type(result.Init) == "function" then
			pcall(result.Init, result)
		end
		if type(result.Build) == "function" then
			pcall(result.Build, result)
		end
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

	-- Tag all captured output and persist build hash for cross-session stale detection
	for _, child: Instance in newChildren do
		CollectionService:AddTag(child, outputTag)
		child:SetAttribute("VertigoBuildHash", sourceHash)
		child:SetAttribute("VertigoBuildPath", path)
	end

	-- Compute dependencies for this builder
	computeBuilderDependencies(path, scriptInstance)

	-- Update tracking state
	BUILDERS.sources[path] = sourceHash
	BUILDERS.outputTags[path] = outputTag

	-- Signal runtime to refresh tag caches
	Workspace:SetAttribute("VertigoBuilderLastRebuild", os.clock())
	Workspace:SetAttribute("VertigoBuilderLastRebuildPath", path)

	info(string.format("Builder executed: %s (hash=%s, captured=%d)", path, string.sub(sourceHash, 1, 8), #newChildren))
end

-- ─── DataModel mutation ─────────────────────────────────────────────────────

@native
local function applyWrite(path: string, source: string, sha256: string?)
	local parent, instanceName, className, boundary = resolveTarget(path)
	if parent == nil or instanceName == nil or className == nil then
		throttledLog("resolve_" .. path, string.format("Cannot resolve target for write: %s", path), true)
		droppedUpdates += 1
		return
	end

	-- Handle StringValue type (e.g. .txt files)
	if className == "StringValue" then
		local inst: Instance = ensureOrCreate(parent, instanceName, "StringValue")
		local stringInst: StringValue = inst :: StringValue
		if stringInst.Value ~= source then
			refreshSelfMutationGuard()
			stringInst.Value = source
		end
		inst:SetAttribute(MANAGED_PATH_ATTR, path)
		if sha256 ~= nil and sha256 ~= "" then
			inst:SetAttribute(MANAGED_SHA_ATTR, sha256)
			managedShaByPath[path] = sha256
		end
		managedIndex[path] = inst
		-- Apply meta if present
		local meta: EntryMeta? = metaByPath[path]
		if meta ~= nil then
			applyMeta(inst, meta)
			metaByPath[path] = nil
		end
		return
	end

	-- Handle LocalizationTable type (e.g. .csv files)
	if className == "LocalizationTable" then
		local inst: Instance = ensureOrCreate(parent, instanceName, "LocalizationTable")
		-- Try SetContents first for proper CSV parsing, fall back to attribute
		local csvOk = pcall(function()
			(inst :: any):SetContents(source)
		end)
		if not csvOk then
			inst:SetAttribute("CSVSource", source)
		end
		inst:SetAttribute(MANAGED_PATH_ATTR, path)
		if sha256 ~= nil and sha256 ~= "" then
			inst:SetAttribute(MANAGED_SHA_ATTR, sha256)
			managedShaByPath[path] = sha256
		end
		managedIndex[path] = inst
		local meta: EntryMeta? = metaByPath[path]
		if meta ~= nil then
			applyMeta(inst, meta)
			metaByPath[path] = nil
		end
		return
	end

	local existingAny = parent:FindFirstChild(instanceName)
	local scriptInstance: LuaSourceContainer

	if existingAny ~= nil and existingAny:IsA("LuaSourceContainer") then
		if existingAny.ClassName ~= className then
			local replaced = replaceInstanceClassPreservingChildren(existingAny, className)
			scriptInstance = replaced :: LuaSourceContainer
		else
			scriptInstance = existingAny
		end
	elseif existingAny ~= nil then
		local replaced = replaceInstanceClassPreservingChildren(existingAny, className)
		scriptInstance = replaced :: LuaSourceContainer
	else
		refreshSelfMutationGuard()
		local created = poolGet(className)
		created.Name = instanceName
		created.Parent = parent
		scriptInstance = created :: LuaSourceContainer
	end

	if scriptInstance.Source ~= source then
		refreshSelfMutationGuard()
		scriptInstance.Source = source
	end

	local runContext: Enum.RunContext? = runContextForPath(path)
	if runContext ~= nil and scriptInstance:IsA("Script") then
		pcall(function()
			(scriptInstance :: Script).RunContext = runContext
		end)
	end

	scriptInstance:SetAttribute(MANAGED_PATH_ATTR, path)
	if sha256 ~= nil and sha256 ~= "" then
		scriptInstance:SetAttribute(MANAGED_SHA_ATTR, sha256)
		managedShaByPath[path] = sha256
	end

	managedIndex[path] = scriptInstance

	-- Apply .meta.json properties if present
	local meta: EntryMeta? = metaByPath[path]
	if meta ~= nil then
		applyMeta(scriptInstance, meta)
		metaByPath[path] = nil
	end

	if boundary ~= nil and scriptInstance.Parent == nil then
		warnMsg(string.format("Write detached unexpectedly for %s under %s", path, boundary:GetFullName()))
	end

	-- Builder re-execution check — direct builder change or dependency cascade
	if BUILDERS.enabled then
		local isBuilder: boolean = false
		local builderPathCount: number = #BUILDER_PATHS
		for i = 1, builderPathCount do
			if string.sub(path, 1, #BUILDER_PATHS[i]) == BUILDER_PATHS[i] then
				isBuilder = true
				break
			end
		end

		if isBuilder then
			-- Direct builder change — add to dirty set and schedule batch
			BUILDERS.dirtySet[path] = true
			scheduleBuilderBatch()
		else
			-- Check if this is a shared dependency that builders depend on
			local depPathCount: number = #BUILDER_DEPENDENCY_PATHS
			for i = 1, depPathCount do
				local depPrefix: string = BUILDER_DEPENDENCY_PATHS[i]
				if string.sub(path, 1, #depPrefix) == depPrefix then
					-- Cascade: find all builders that depend on this prefix
					local dependentBuilders: { [string]: boolean }? = BUILDERS.dependencyMap[depPrefix]
					if dependentBuilders ~= nil then
						for builderPath: string in dependentBuilders do
							BUILDERS.dirtySet[builderPath] = true
						end
						scheduleBuilderBatch()
					end
					break
				end
			end
		end
	end
end

@native
local function applyDelete(path: string)
	local _parent, _instanceName, _className, boundary = resolveTarget(path)
	local existing = managedIndex[path]
	if existing ~= nil and existing.Parent ~= nil then
		local parent = existing.Parent
		refreshSelfMutationGuard()
		poolReturn(existing)
		managedIndex[path] = nil
		managedShaByPath[path] = nil
		metaByPath[path] = nil
		cleanupEmptyAncestors(parent, boundary)
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
		poolReturn(target)
		cleanupEmptyAncestors(targetParent, boundary)
	end
	managedIndex[path] = nil
	managedShaByPath[path] = nil
	metaByPath[path] = nil
end

-- ─── Pending operation coalescing ───────────────────────────────────────────

@native
local function stageOperation(path: string, action: PendingAction, expectedSha: string?)
	local mapping, _remainder = resolveMapping(path)
	if mapping == nil then
		throttledLog("unmappable_" .. path, "Dropped unmappable path: " .. path, true)
		droppedUpdates += 1
		return
	end

	-- Skip binary model entries if feature is disabled
	local fileType: string? = fileTypeForPath(path)
	if isBinaryModelType(fileType) and not SETTINGS.binaryModels then
		return
	end

	-- Binary model delete: clean up model instances
	if isBinaryModelType(fileType) and action == "delete" then
		cleanupModelInstances(path)
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

	enqueuePath(path)

	if action == "write" then
		-- Binary models: spawn a dedicated manifest fetch instead of using the source fetch queue
		if isBinaryModelType(fileType) then
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
						stageDelete(path)
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

				stageModelManifest(path, manifest, capturedEpoch, capturedSha)
			end)
		else
			pushFetchTask(path, currentEpoch)
		end
	end
end

@native
local function stageWrite(path: string, expectedSha: string?)
	stageOperation(path, "write", expectedSha)
end

@native
local function stageDelete(path: string)
	stageOperation(path, "delete", nil)
end

--- Rename an existing managed instance instead of deleting + recreating.
--- Preserves instance references, runtime state, and selection state.
--- Falls back to delete + write if the source instance is missing.
local function stageRename(oldPath: string, newPath: string, sha256: string?)
	local existing: Instance? = managedIndex[oldPath]
	if existing == nil or (existing :: Instance).Parent == nil then
		-- Fallback: treat as delete + add
		stageDelete(oldPath)
		stageWrite(newPath, sha256)
		return
	end

	-- Resolve new target location
	local newParent, newInstanceName, newClassName, newBoundary = resolveTarget(newPath)
	if newParent == nil or newInstanceName == nil then
		stageDelete(oldPath)
		stageWrite(newPath, sha256)
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
	inst:SetAttribute(MANAGED_PATH_ATTR, newPath)
	managedIndex[newPath] = inst
	managedIndex[oldPath] = nil
	if sha256 then
		inst:SetAttribute(MANAGED_SHA_ATTR, sha256)
		managedShaByPath[newPath] = sha256
	end
	managedShaByPath[oldPath] = nil

	-- Migrate meta cache
	if metaByPath[oldPath] ~= nil then
		metaByPath[newPath] = metaByPath[oldPath]
		metaByPath[oldPath] = nil
	end

	-- Clean up empty ancestor folders from old location
	local _oldParent, _oldName, _oldClass, oldBoundary = resolveTarget(oldPath)
	if _oldParent and _oldParent ~= inst.Parent then
		cleanupEmptyAncestors(_oldParent, oldBoundary)
	end
end

@native
local function stagePaths(paths: any, action: PendingAction)
	if type(paths) ~= "table" then
		return
	end
	local pathCount: number = #paths
	for i = 1, pathCount do
		local rawPath: any = paths[i]
		if type(rawPath) == "string" and rawPath ~= "" then
			if action == "write" then
				stageWrite(rawPath, nil)
			else
				stageDelete(rawPath)
			end
		end
	end
end

-- ─── Snapshot + diff reconciliation ─────────────────────────────────────────

@native
local function reconcileSnapshot(snapshot: SnapshotResponse)
	local entries: { SnapshotEntry } = snapshot.entries
	local entryCount: number = #entries
	local seenPaths: { [string]: boolean } = {}

	for i = 1, entryCount do
		local entry: SnapshotEntry = entries[i]
		local entryPath: string = entry.path
		local entrySha: string = entry.sha256
		seenPaths[entryPath] = true

		-- Cache meta for later application during applyWrite
		if entry.meta ~= nil then
			metaByPath[entryPath] = entry.meta
		end

		if managedShaByPath[entryPath] ~= entrySha then
			stageWrite(entryPath, entrySha)
		end
	end

	for path: string, _sha: string in pairs(managedShaByPath) do
		if not seenPaths[path] then
			stageDelete(path)
		end
	end

	lastHash = snapshot.fingerprint
	setStatusAttributes("connected", snapshot.fingerprint)
end

@native
local function beginFullResync()
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

local function syncFromSnapshot(reason: string): boolean
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
	beginFullResync()
	reconcileSnapshot(snapshot)
	resyncRequested = false
	consecutiveErrors = 0
	pollInterval = POLL_INTERVAL_FAST
	info(string.format("Snapshot reconciled (%s). fingerprint=%s entries=%d", reason, snapshot.fingerprint, #snapshot.entries))
	return true
end

@native
local function pollDiff()
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
		pollInterval = math.min(POLL_INTERVAL_MAX, pollInterval * 1.6)
		throttledLog("diff_poll_fail", string.format("Diff poll failed (attempt=%d): %s", consecutiveErrors, tostring(payloadOrErr)), true)
		if consecutiveErrors >= 5 then
			setStatusAttributes("error", lastHash)
		end
		return
	end

	consecutiveErrors = 0
	pollInterval = POLL_INTERVAL_FAST

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

	if type(diff.added) == "table" then
		for _, entry in ipairs(diff.added) do
			if type(entry) == "table" and type(entry.path) == "string" then
				if entry.meta ~= nil then
					metaByPath[entry.path] = entry.meta
				end
				stageWrite(entry.path, entry.sha256)
			end
		end
	end
	if type(diff.modified) == "table" then
		for _, entry in ipairs(diff.modified) do
			if type(entry) == "table" and type(entry.path) == "string" then
				if entry.meta ~= nil then
					metaByPath[entry.path] = entry.meta
				end
				stageWrite(entry.path, entry.current_sha256)
			end
		end
	end
	if type(diff.deleted) == "table" then
		for _, entry in ipairs(diff.deleted) do
			if type(entry) == "table" and type(entry.path) == "string" then
				stageDelete(entry.path)
			end
		end
	end
	-- Process renames: move instance in-place instead of delete+recreate
	if type(diff.renamed) == "table" then
		for _, entry in ipairs(diff.renamed) do
			if type(entry) == "table" and type(entry.old_path) == "string" and type(entry.new_path) == "string" then
				stageRename(entry.old_path, entry.new_path, entry.sha256)
			end
		end
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
local function processFetchQueue()
	local function scheduleFetchRetry(path: string, epoch: number, reason: string?)
		local current = pendingOps[path]
		if current == nil or current.action ~= "write" or current.epoch ~= epoch then
			return
		end

		current.retries += 1
		if current.retries > MAX_SOURCE_FETCH_RETRIES then
			droppedUpdates += 1
			throttledLog("fetch_exhaust_" .. path, string.format("Source fetch retries exhausted for %s (%s)", path, tostring(reason)), true)
			resyncRequested = true
			return
		end

		local backoff = math.min(0.15 * current.retries, 0.75)
		task.delay(backoff, function()
			local stillCurrent = pendingOps[path]
			if stillCurrent and stillCurrent.action == "write" and stillCurrent.epoch == epoch then
				pushFetchTask(path, epoch)
			end
		end)
	end

	while fetchInFlight < adaptiveFetchConcurrency do
		local availableSlots = math.max(adaptiveFetchConcurrency - fetchInFlight, 0)
		if availableSlots <= 0 then
			return
		end

		local batchCap = clampNumber(availableSlots, 1, MAX_SOURCE_BATCH_SIZE)
		local batchPaths: { string } = table.create(batchCap)
		local batchEpochByPath: { [string]: number } = {}

		while #batchPaths < batchCap do
			local taskItem = popFetchTask()
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
					enqueuePath(path)
					return
				end

				if statusCode == 404 then
					stageDelete(path)
					return
				end

				scheduleFetchRetry(path, epoch, err)
				return
			end

			local ok, payload, statusCode, err = requestSourcesBatch(batchPaths)
			completeInflight()

			if not ok or payload == nil then
				if statusCode == 413 then
					-- Payload too large: shrink concurrency so future batches are smaller.
					adaptiveFetchConcurrency = FETCH_CONCURRENCY_MIN
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
					enqueuePath(path)
				elseif missingByPath[path] then
					stageDelete(path)
				else
					scheduleFetchRetry(path, epoch, err or "batch response missing path")
				end
			end
		end)
	end
end

-- ─── Apply loop ─────────────────────────────────────────────────────────────

@native
local function recalcAdaptiveThresholds(appliedThisTick: number, tickElapsedSeconds: number)
	if appliedThisTick > 0 and tickElapsedSeconds > 0 then
		local perApplySeconds = tickElapsedSeconds / appliedThisTick
		if applyCostEwmaSeconds <= 0 then
			applyCostEwmaSeconds = perApplySeconds
		else
			local alpha = APPLY_BUDGET_EWMA_ALPHA
			applyCostEwmaSeconds = applyCostEwmaSeconds * (1 - alpha) + perApplySeconds * alpha
		end
	elseif applyCostEwmaSeconds <= 0 then
		applyCostEwmaSeconds = APPLY_FRAME_BUDGET_SECONDS / MAX_APPLIES_PER_TICK
	end

	local now = os.clock()
	if now - lastAdaptiveRecalcAt < APPLY_BUDGET_RECALC_SECONDS then
		return
	end
	lastAdaptiveRecalcAt = now

	local pendingDepth = math.max(#pendingQueue - pendingQueueHead + 1, 0)
	local fetchDepth = math.max(#fetchQueue - fetchQueueHead + 1, 0)
	local backlogRatio = clampNumber(pendingDepth / APPLY_QUEUE_HIGH_WATERMARK, 0, 4)

	local targetBudget = APPLY_FRAME_BUDGET_SECONDS * (1 + 0.5 * backlogRatio)
	adaptiveApplyBudgetSeconds = clampNumber(targetBudget, APPLY_FRAME_BUDGET_MIN_SECONDS, APPLY_FRAME_BUDGET_MAX_SECONDS)

	local opCost = if applyCostEwmaSeconds > 0
		then applyCostEwmaSeconds
		else (APPLY_FRAME_BUDGET_SECONDS / MAX_APPLIES_PER_TICK)

	local budgetedOps = math.floor((adaptiveApplyBudgetSeconds / opCost) * 0.9 + 0.5)
	local backlogBoost = math.floor(pendingDepth / 96)
	local targetMaxApplies = budgetedOps + backlogBoost
	targetMaxApplies = clampNumber(targetMaxApplies, APPLY_MIN_APPLIES_PER_TICK, APPLY_MAX_APPLIES_HARD_LIMIT)
	adaptiveMaxAppliesPerTick = math.floor(targetMaxApplies + 0.5)

	local fetchBoost = math.floor(fetchDepth / 64)
	local targetFetchConcurrency = MAX_FETCH_CONCURRENCY + fetchBoost
	targetFetchConcurrency = clampNumber(targetFetchConcurrency, FETCH_CONCURRENCY_MIN, FETCH_CONCURRENCY_MAX)
	adaptiveFetchConcurrency = math.floor(targetFetchConcurrency + 0.5)
end

@native
local function processApplyQueue()
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

		local path: string? = popPendingPath()
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
				enqueuePath(path)
			end
			continue
		end

		local ready: ReadySource? = readySources[path]
		if ready == nil or ready.epoch ~= op.epoch then
			if inflightFetchEpoch[path] ~= op.epoch then
				pushFetchTask(path, op.epoch)
			end
			continue
		end

		-- Store meta from ready source for applyWrite to consume
		if ready.meta ~= nil then
			metaByPath[path] = ready.meta
		end

		local writeOk: boolean, writeErr: any = pcall(applyWrite, path, ready.source, ready.sha256 or op.expectedSha)
		if writeOk then
			pendingOps[path] = nil
			readySources[path] = nil
			appliedThisTick += 1
		else
			op.retries += 1
			if op.retries > MAX_SOURCE_FETCH_RETRIES then
				droppedUpdates += 1
				warnMsg(string.format("Write apply permanently failed for %s after %d retries: %s", path, op.retries, tostring(writeErr)))
				pendingOps[path] = nil
				readySources[path] = nil
			else
				-- Retry: keep the ready source, re-enqueue for next tick
				warnMsg(string.format("Write apply failed for %s (retry %d): %s", path, op.retries, tostring(writeErr)))
				op.queued = false
				task.defer(function()
					local stillCurrent = pendingOps[path]
					if stillCurrent and stillCurrent.epoch == op.epoch then
						enqueuePath(path)
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
	recalcAdaptiveThresholds(appliedThisTick, tickElapsed)
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
	if not force and HISTORY.loaded and (now - HISTORY.lastFetchAt) < HISTORY_REFRESH_INTERVAL_SECONDS then
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
			table.insert(validated, entry :: HistoryEntry)
		end
	end
	HISTORY.entries = validated
	HISTORY.loaded = true
	HISTORY.fetchFailed = false
	return true
end

function TimeTravel.rewindToIndex(targetIndex: number)
	if targetIndex < 1 or targetIndex > #HISTORY.entries or HISTORY.busy then
		return
	end
	if HISTORY.active and HISTORY.currentIndex == targetIndex then
		return
	end

	local targetFingerprint: string = HISTORY.entries[targetIndex].fingerprint
	HISTORY.active = true
	HISTORY.busy = true

	-- Pause normal sync
	syncEnabled = false

	-- Request reverse diff from server
	local ok: boolean, payload: any = requestJson("/rewind?to=" .. HttpService:UrlEncode(targetFingerprint))
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

	-- Apply the reverse diff through the normal pipeline
	local diff = payload :: DiffResponse
	beginFullResync()

	-- Stage operations from the reverse diff
	if type(diff.added) == "table" then
		for _, entry in ipairs(diff.added) do
			if type(entry) == "table" and type(entry.path) == "string" then
				stageWrite(entry.path, entry.sha256)
			end
		end
	end
	if type(diff.modified) == "table" then
		for _, entry in ipairs(diff.modified) do
			if type(entry) == "table" and type(entry.path) == "string" then
				stageWrite(entry.path, entry.current_sha256)
			end
		end
	end
	if type(diff.deleted) == "table" then
		for _, entry in ipairs(diff.deleted) do
			if type(entry) == "table" and type(entry.path) == "string" then
				stageDelete(entry.path)
			end
		end
	end
	if type(diff.renamed) == "table" then
		for _, entry in ipairs(diff.renamed) do
			if type(entry) == "table" and type(entry.old_path) == "string" and type(entry.new_path) == "string" then
				stageRename(entry.old_path, entry.new_path, entry.sha256)
			end
		end
	end

	lastHash = diff.current_fingerprint
	HISTORY.currentIndex = targetIndex

	setStatusAttributes("connected", diff.current_fingerprint)
	Workspace:SetAttribute("VertigoSyncTimeTravel", true)
	Workspace:SetAttribute("VertigoSyncTimeTravelSeq", HISTORY.entries[targetIndex].seq)

	info(string.format("Rewound to seq %d (fingerprint=%s)", HISTORY.entries[targetIndex].seq, targetFingerprint))
	HISTORY.busy = false
end

function TimeTravel.resumeLiveSync()
	if HISTORY.busy then
		return
	end
	HISTORY.active = false
	HISTORY.currentIndex = 0
	syncEnabled = true
	resyncRequested = true -- Force full resync to get back to current state
	Workspace:SetAttribute("VertigoSyncTimeTravel", false)
	Workspace:SetAttribute("VertigoSyncTimeTravelSeq", nil)
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

-- Everything below runs inside a function to create a new register scope.
-- Luau has a 200 local register limit per function; do/end does NOT help.
local function _initPlugin()

local function runInitialBuilders()
	if not BUILDERS.enabled then
		return
	end
	if not isEditMode() then
		return
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
		local builderPathCount: number = #BUILDER_PATHS
		for i = 1, builderPathCount do
			local builderPrefix: string = BUILDER_PATHS[i]
			for path: string, inst: Instance in pairs(managedIndex) do
				if string.sub(path, 1, #builderPrefix) == builderPrefix and inst:IsA("LuaSourceContainer") then
					computeBuilderDependencies(path, inst :: LuaSourceContainer)
					-- Check if this builder's output hash matches current source
					local currentSha: string = inst:GetAttribute(MANAGED_SHA_ATTR) or ""
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
			scheduleBuilderBatch()
		else
			info("All builders current — no rebuilds needed.")
		end
		return
	end

	local totalStart: number = os.clock()
	local builderList: { { path: string, inst: LuaSourceContainer } } = {}

	local builderPathCount: number = #BUILDER_PATHS
	for i = 1, builderPathCount do
		local builderPrefix: string = BUILDER_PATHS[i]
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

	local executed: number = 0
	local skipped: number = 0

	for _, entry: { path: string, inst: LuaSourceContainer } in builderList do
		local path: string = entry.path
		local inst: LuaSourceContainer = entry.inst
		local sourceHash: string = inst:GetAttribute(MANAGED_SHA_ATTR) or "unknown"

		-- Skip if output already exists with matching hash
		local existingTag: string? = BUILDERS.outputTags[path]
		if existingTag ~= nil then
			local segments: { string } = string.split(path, "/")
			local joined: string = table.concat(segments, "_")
			local parts: { string } = string.split(joined, ".")
			local sanitized: string = table.concat(parts, "_")
			local expectedTag: string = "BuilderOutput_" .. sanitized .. "_" .. string.sub(sourceHash, 1, 8)
			if existingTag == expectedTag then
				local existingOutputs: { Instance } = CollectionService:GetTagged(expectedTag)
				if #existingOutputs > 0 then
					skipped += 1
					-- Still compute dependencies even for skipped builders
					computeBuilderDependencies(path, inst)
					continue
				end
			end
		end

		local builderStart: number = os.clock()
		executeBuilder(path, inst)
		local builderElapsed: number = os.clock() - builderStart
		executed += 1
		info(string.format("  Builder [%d/%d] %s took %.1fms", executed + skipped, #builderList, path, builderElapsed * 1000))
	end

	local totalElapsed: number = os.clock() - totalStart
	info(string.format("Initial builders complete: %d executed, %d skipped in %.1fms", executed, skipped, totalElapsed * 1000))

	-- Signal ZoneService to skip builders on Play
	Workspace:SetAttribute("VertigoSyncWorldReady", true)
end

-- ─── Health / transport ─────────────────────────────────────────────────────

@native
local function checkHealth(): boolean
	local ok, payloadOrErr = requestJson("/health")
	if ok then
		consecutiveErrors = 0
		-- Detect server restart via server_boot_time
		if type(payloadOrErr) == "table" and type(payloadOrErr.server_boot_time) == "number" then
			local reportedBootTime: number = payloadOrErr.server_boot_time
			if serverBootTimeCache ~= nil and reportedBootTime ~= serverBootTimeCache then
				-- Server restarted — trigger resync
				throttledLog("server_restart", string.format("Server restart detected (boot_time %d -> %d), requesting resync", serverBootTimeCache :: number, reportedBootTime), false)
				resyncRequested = true
				PROJECT.loaded = false
				setProjectStatus("bootstrapping", "Refreshing /project after server restart", PROJECT.name, false)
			end
			serverBootTimeCache = reportedBootTime
		end
		return true
	end
	consecutiveErrors += 1
	throttledLog("health_fail", string.format("Health check failed (attempt=%d): %s", consecutiveErrors, tostring(payloadOrErr)), true)
	return false
end

local function closeWebSocket(reason: string)
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

local function scheduleWsReconnect()
	local now = os.clock()
	nextWsConnectAt = now + wsReconnectBackoffSeconds + math.random() * 0.15
	wsReconnectBackoffSeconds = math.min(WS_RECONNECT_MAX_SECONDS, wsReconnectBackoffSeconds * 1.7)
end

@native
local function onWsMessage(rawText: string)
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
		wsReconnectBackoffSeconds = WS_RECONNECT_MIN_SECONDS

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
			stagePaths(paths.added, "write")
			stagePaths(paths.modified, "write")
			stagePaths(paths.deleted, "delete")
			-- Process renames: move instance in-place instead of delete+recreate
			if type(paths.renamed) == "table" then
				for _, entry in ipairs(paths.renamed) do
					if type(entry) == "table" and type(entry.old_path) == "string" and type(entry.new_path) == "string" then
						stageRename(entry.old_path, entry.new_path, nil)
					end
				end
			end
		end
	end
end

local function tryConnectWebSocket()
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
		scheduleWsReconnect()
		warnMsg(string.format("WS connect failed (%s); falling back to polling", tostring(socketOrErr)))
		return false
	end

	local socket = socketOrErr
	wsSocket = socket
	transportMode = "ws"
	reconnectCount += 1

	(socket :: any).MessageReceived:Connect(function(message: string)
		onWsMessage(message)
	end)

	if (socket :: any).Closed ~= nil then
		(socket :: any).Closed:Connect(function()
			closeWebSocket("closed")
			scheduleWsReconnect()
		end)
	end

	info("WebSocket connected: realtime streaming enabled")
	return true
end

-- ─── DockWidget UI ──────────────────────────────────────────────────────────
-- Wrapped in do...end to scope UI locals within Luau's 200 local register limit.


local WIDGET_ID = "VertigoSyncWidget"
local widgetInfo = DockWidgetPluginGuiInfo.new(
	Enum.InitialDockState.Right,
	false, -- initially disabled
	true, -- override previous enabled state (remembers user's last open/close)
	340, -- default width
	400, -- default height
	300, -- min width
	300 -- min height
)
local widget: DockWidgetPluginGui = plugin:CreateDockWidgetPluginGui(WIDGET_ID, widgetInfo)
widget.Title = "Vertigo Sync"

-- ─── Settings Persistence ────────────────────────────────────────────────────

local function loadSettings()
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
end

local function saveSetting(key: string, value: any)
	pcall(function()
		plugin:SetSetting(key, value)
	end)
end

loadSettings()

-- ─── UI Design System ────────────────────────────────────────────────────────
-- Trillion-dollar quality: 8px grid, Linear/Figma/Apple-grade polish

local THEME_BG = Color3.fromRGB(30, 30, 30)
local THEME_SURFACE = Color3.fromRGB(38, 38, 38)
local THEME_SURFACE_ELEVATED = Color3.fromRGB(46, 46, 46)
local THEME_BORDER = Color3.fromRGB(60, 60, 60)
local THEME_TEXT = Color3.fromRGB(220, 220, 220)
local THEME_TEXT_DIM = Color3.fromRGB(140, 140, 140)
local THEME_ACCENT = Color3.fromRGB(56, 132, 244)
local THEME_GREEN = Color3.fromRGB(52, 199, 89)
local THEME_YELLOW = Color3.fromRGB(255, 159, 10)
local THEME_RED = Color3.fromRGB(255, 69, 58)

-- Tween presets
local TWEEN_FAST = TweenInfo.new(0.15, Enum.EasingStyle.Quad, Enum.EasingDirection.Out)
local TWEEN_MEDIUM = TweenInfo.new(0.2, Enum.EasingStyle.Quad, Enum.EasingDirection.Out)
local TWEEN_SLOW = TweenInfo.new(0.3, Enum.EasingStyle.Quad, Enum.EasingDirection.Out)
local TWEEN_POP = TweenInfo.new(0.25, Enum.EasingStyle.Back, Enum.EasingDirection.Out)
local TWEEN_PULSE = TweenInfo.new(3.2, Enum.EasingStyle.Sine, Enum.EasingDirection.InOut, -1, true)

-- Theme-relative hover/press colors (computed once, not hardcoded)
local THEME_HOVER = Color3.fromRGB(
	math.min(255, math.floor(46 * 1.22)),
	math.min(255, math.floor(46 * 1.22)),
	math.min(255, math.floor(46 * 1.22))
)
local THEME_PRESS = Color3.fromRGB(
	math.max(0, math.floor(46 * 0.74)),
	math.max(0, math.floor(46 * 0.74)),
	math.max(0, math.floor(46 * 0.74))
)

-- Helper: create text label with design system styling
local function createLabel(parent: Instance, name: string, text: string, props: {
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
	label.TextColor3 = p.color or THEME_TEXT
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

-- Helper: create a panel (surface card)
local function createPanel(parent: Instance, name: string, layoutOrder: number, height: number?): Frame
	local panel: Frame = Instance.new("Frame")
	panel.Name = name
	panel.Size = UDim2.new(1, 0, 0, height or 0)
	panel.AutomaticSize = if height then Enum.AutomaticSize.None else Enum.AutomaticSize.Y
	panel.BackgroundColor3 = THEME_SURFACE
	panel.BorderSizePixel = 0
	panel.LayoutOrder = layoutOrder
	local corner: UICorner = Instance.new("UICorner")
	corner.CornerRadius = UDim.new(0, 6)
	corner.Parent = panel
	-- Bottom border shadow
	local stroke: UIStroke = Instance.new("UIStroke")
	stroke.Color = THEME_BORDER
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

-- Helper: create a toggle switch (32x18 track, 14x14 thumb)
local function createToggleSwitch(parent: Instance, name: string, labelText: string, initialState: boolean, layoutOrder: number): (Frame, Frame, TextLabel)
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
	label.TextColor3 = THEME_TEXT
	label.TextSize = 12
	label.Font = Enum.Font.RobotoMono
	label.TextXAlignment = Enum.TextXAlignment.Left
	label.TextYAlignment = Enum.TextYAlignment.Center
	label.Parent = row

	local track: Frame = Instance.new("Frame")
	track.Name = "Track"
	track.Size = UDim2.new(0, 32, 0, 18)
	track.Position = UDim2.new(1, -32, 0.5, -9)
	track.BackgroundColor3 = if initialState then THEME_ACCENT else THEME_BG
	track.BorderSizePixel = 0
	local trackCorner: UICorner = Instance.new("UICorner")
	trackCorner.CornerRadius = UDim.new(1, 0)
	trackCorner.Parent = track
	local trackStroke: UIStroke = Instance.new("UIStroke")
	trackStroke.Color = THEME_BORDER
	trackStroke.Transparency = 0.4
	trackStroke.Thickness = 1
	trackStroke.Parent = track
	track.Parent = row

	-- Thumb shadow (subtle depth)
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

	-- Click region over the track
	local clickBtn: TextButton = Instance.new("TextButton")
	clickBtn.Name = "ClickRegion"
	clickBtn.Size = UDim2.new(1, 0, 1, 0)
	clickBtn.BackgroundTransparency = 1
	clickBtn.Text = ""
	clickBtn.Parent = track

	return row, track, label
end

-- Helper: animate toggle switch state
local function animateToggle(track: Frame, state: boolean)
	local thumb: Frame? = track:FindFirstChild("Thumb") :: Frame?
	local thumbShadow: Frame? = track:FindFirstChild("ThumbShadow") :: Frame?
	if thumb == nil then
		return
	end
	TweenService:Create(track, TWEEN_FAST, {
		BackgroundColor3 = if state then THEME_ACCENT else THEME_BG,
	}):Play()
	TweenService:Create(thumb, TWEEN_FAST, {
		Position = if state then UDim2.new(1, -16, 0.5, -7) else UDim2.new(0, 2, 0.5, -7),
	}):Play()
	if thumbShadow then
		TweenService:Create(thumbShadow, TWEEN_FAST, {
			Position = if state then UDim2.new(1, -15, 0.5, -6) else UDim2.new(0, 3, 0.5, -6),
		}):Play()
	end
end

-- Helper: create a small button
local function createSmallButton(parent: Instance, name: string, text: string, width: number): TextButton
	local btn: TextButton = Instance.new("TextButton")
	btn.Name = name
	btn.Text = text
	btn.Size = UDim2.new(0, width, 0, 24)
	btn.BackgroundColor3 = THEME_SURFACE_ELEVATED
	btn.TextColor3 = THEME_TEXT
	btn.TextSize = 12
	btn.Font = Enum.Font.RobotoMono
	btn.AutoButtonColor = false
	btn.BorderSizePixel = 0
	local corner: UICorner = Instance.new("UICorner")
	corner.CornerRadius = UDim.new(0, 4)
	corner.Parent = btn
	-- Hover/press states
	btn.MouseEnter:Connect(function()
		TweenService:Create(btn, TWEEN_FAST, { BackgroundColor3 = THEME_HOVER }):Play()
	end)
	btn.MouseLeave:Connect(function()
		TweenService:Create(btn, TWEEN_FAST, { BackgroundColor3 = THEME_SURFACE_ELEVATED }):Play()
	end)
	btn.MouseButton1Down:Connect(function()
		TweenService:Create(btn, TWEEN_FAST, { BackgroundColor3 = THEME_PRESS }):Play()
	end)
	btn.MouseButton1Up:Connect(function()
		TweenService:Create(btn, TWEEN_FAST, { BackgroundColor3 = THEME_SURFACE_ELEVATED }):Play()
	end)
	btn.Parent = parent
	return btn
end

-- ─── Build Widget UI ─────────────────────────────────────────────────────────

-- Scrollable container (handles small widget sizes gracefully)
local scrollFrame: ScrollingFrame = Instance.new("ScrollingFrame")
scrollFrame.Name = "ScrollContainer"
scrollFrame.Size = UDim2.new(1, 0, 1, 0)
scrollFrame.BackgroundColor3 = THEME_BG
scrollFrame.BorderSizePixel = 0
scrollFrame.ScrollBarThickness = 3
scrollFrame.ScrollBarImageColor3 = THEME_BORDER
scrollFrame.ScrollBarImageTransparency = 0.6
scrollFrame.AutomaticCanvasSize = Enum.AutomaticSize.Y
scrollFrame.CanvasSize = UDim2.new(0, 0, 0, 0)
scrollFrame.Parent = widget

-- ═══ Toast Notification System (pre-allocated pool of 3) ═══════════════════

local TOAST_COUNT = 3
local TOAST_DISMISS_SECONDS = 2.0
local TOAST_HEIGHT = 28
local TOAST_GAP = 4

local toastContainer: Frame = Instance.new("Frame")
toastContainer.Name = "ToastContainer"
toastContainer.Size = UDim2.new(1, -16, 0, (TOAST_HEIGHT + TOAST_GAP) * TOAST_COUNT)
toastContainer.Position = UDim2.new(0, 8, 1, -((TOAST_HEIGHT + TOAST_GAP) * TOAST_COUNT) - 8)
toastContainer.BackgroundTransparency = 1
toastContainer.ZIndex = 10
toastContainer.Parent = widget

local toastFrames: { Frame } = table.create(TOAST_COUNT)
local toastLabels: { TextLabel } = table.create(TOAST_COUNT)
local toastActive: { boolean } = table.create(TOAST_COUNT, false)
local toastDismissAt: { number } = table.create(TOAST_COUNT, 0)
local toastNextSlot = 1

for i = 1, TOAST_COUNT do
	local slot: Frame = Instance.new("Frame")
	slot.Name = "Toast" .. tostring(i)
	slot.Size = UDim2.new(1, 0, 0, TOAST_HEIGHT)
	slot.Position = UDim2.new(0, 0, 1, -((i) * (TOAST_HEIGHT + TOAST_GAP)))
	slot.BackgroundColor3 = THEME_SURFACE_ELEVATED
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
	slotLabel.Size = UDim2.new(1, 0, 1, 0)
	slotLabel.BackgroundTransparency = 1
	slotLabel.TextColor3 = THEME_TEXT
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

-- Re-assign toast colors to use theme values (forward-declared above)
TOAST_COLOR_SUCCESS = THEME_GREEN
TOAST_COLOR_ERROR = THEME_RED
TOAST_COLOR_INFO = THEME_ACCENT

-- Toast dedup state (suppress identical messages within 2s)
local lastToastMessage: string = ""
local lastToastAt: number = 0

-- Assign the real showToast implementation (forward-declared above)
showToast = function(message: string, toastColor: Color3?)
	-- Dedup: suppress identical toast within 2 seconds
	local now: number = os.clock()
	if message == lastToastMessage and now - lastToastAt < 2.0 then
		return
	end
	lastToastMessage = message
	lastToastAt = now
	local color: Color3 = toastColor or TOAST_COLOR_INFO
	local slot: number = toastNextSlot
	toastNextSlot = (toastNextSlot % TOAST_COUNT) + 1

	local frame: Frame = toastFrames[slot]
	local label: TextLabel = toastLabels[slot]

	label.Text = message
	frame.BackgroundColor3 = color
	frame.BackgroundTransparency = 1
	frame.Position = UDim2.new(0, 0, 1, 0) -- start off-screen bottom

	-- Target position in the stack
	local targetY: number = -((slot) * (TOAST_HEIGHT + TOAST_GAP))

	-- Pop in with slight overshoot (Back easing)
	TweenService:Create(frame, TWEEN_POP, {
		BackgroundTransparency = 0.1,
		Position = UDim2.new(0, 0, 1, targetY),
	}):Play()

	toastActive[slot] = true
	toastDismissAt[slot] = os.clock() + TOAST_DISMISS_SECONDS
end

-- Toast dismiss loop runs on a separate task to avoid Heartbeat overhead
task.spawn(function()
	while true do
		local now: number = os.clock()
		for i = 1, TOAST_COUNT do
			if toastActive[i] and now >= toastDismissAt[i] then
				toastActive[i] = false
				TweenService:Create(toastFrames[i], TWEEN_SLOW, {
					BackgroundTransparency = 1,
				}):Play()
				-- Also fade out label
				TweenService:Create(toastLabels[i], TWEEN_SLOW, {
					TextTransparency = 1,
				}):Play()
				-- Reset label transparency after tween completes
				task.delay(0.35, function()
					if not toastActive[i] then
						toastLabels[i].TextTransparency = 0
					end
				end)
			end
		end
		task.wait(0.1)
	end
end)

-- Main frame (inside scroll)
local mainFrame: Frame = Instance.new("Frame")
mainFrame.Name = "MainFrame"
mainFrame.Size = UDim2.new(1, 0, 0, 0)
mainFrame.AutomaticSize = Enum.AutomaticSize.Y
mainFrame.BackgroundColor3 = THEME_BG
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

-- ═══ Welcome Screen (shown only when never-connected) ═══════════════════════

local welcomeFrame: Frame = Instance.new("Frame")
welcomeFrame.Name = "WelcomeFrame"
welcomeFrame.Size = UDim2.new(1, 0, 0, 0)
welcomeFrame.AutomaticSize = Enum.AutomaticSize.Y
welcomeFrame.BackgroundColor3 = THEME_SURFACE
welcomeFrame.BorderSizePixel = 0
welcomeFrame.LayoutOrder = 0
welcomeFrame.ClipsDescendants = true
local welcomeCorner: UICorner = Instance.new("UICorner")
welcomeCorner.CornerRadius = UDim.new(0, 6)
welcomeCorner.Parent = welcomeFrame
local welcomeStroke: UIStroke = Instance.new("UIStroke")
welcomeStroke.Color = THEME_BORDER
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

local welcomeHeader: TextLabel = createLabel(welcomeFrame, "WelcomeHeader", "Welcome to Vertigo Sync", {
	size = UDim2.new(1, 0, 0, 20),
	color = THEME_TEXT,
	fontSize = 15,
	font = Enum.Font.GothamBold,
	layoutOrder = 1,
})

local welcomeStep1: TextLabel = createLabel(welcomeFrame, "Step1", "1. Install:", {
	size = UDim2.new(1, 0, 0, 14),
	color = THEME_TEXT_DIM,
	fontSize = 12,
	font = Enum.Font.Gotham,
	layoutOrder = 2,
})

local welcomeCmd1: TextLabel = createLabel(welcomeFrame, "Cmd1", "   cargo install vertigo-sync", {
	size = UDim2.new(1, 0, 0, 14),
	color = THEME_ACCENT,
	fontSize = 11,
	font = Enum.Font.RobotoMono,
	layoutOrder = 3,
	wrap = true,
})

local welcomeStep2: TextLabel = createLabel(welcomeFrame, "Step2", "2. Start:", {
	size = UDim2.new(1, 0, 0, 14),
	color = THEME_TEXT_DIM,
	fontSize = 12,
	font = Enum.Font.Gotham,
	layoutOrder = 4,
})

local welcomeCmd2: TextLabel = createLabel(welcomeFrame, "Cmd2", "   vertigo-sync serve --turbo", {
	size = UDim2.new(1, 0, 0, 14),
	color = THEME_ACCENT,
	fontSize = 11,
	font = Enum.Font.RobotoMono,
	layoutOrder = 5,
	wrap = true,
})

local welcomeStep3: TextLabel = createLabel(welcomeFrame, "Step3", "3. This panel connects automatically", {
	size = UDim2.new(1, 0, 0, 14),
	color = THEME_TEXT_DIM,
	fontSize = 12,
	font = Enum.Font.Gotham,
	layoutOrder = 6,
	wrap = true,
})

-- "Check Connection" button
local welcomeCheckBtn: TextButton = createSmallButton(welcomeFrame, "CheckConnection", "Check Connection", 130)
welcomeCheckBtn.LayoutOrder = 7

-- "Learn more" link text
local welcomeLearnMore: TextButton = Instance.new("TextButton")
welcomeLearnMore.Name = "LearnMore"
welcomeLearnMore.Text = "Learn more"
welcomeLearnMore.Size = UDim2.new(0, 80, 0, 14)
welcomeLearnMore.BackgroundTransparency = 1
welcomeLearnMore.TextColor3 = THEME_ACCENT
welcomeLearnMore.TextSize = 11
welcomeLearnMore.Font = Enum.Font.Gotham
welcomeLearnMore.TextXAlignment = Enum.TextXAlignment.Left
welcomeLearnMore.LayoutOrder = 8
welcomeLearnMore.AutoButtonColor = false
welcomeLearnMore.Parent = welcomeFrame

welcomeFrame.Parent = mainFrame
welcomeFrame.Visible = false -- controlled by connection state machine

-- ═══ Status Panel ═══════════════════════════════════════════════════════════

local statusPanel: Frame = createPanel(mainFrame, "StatusPanel", 1, 58)

-- Status row: dot + text
local statusDot: Frame = Instance.new("Frame")
statusDot.Name = "StatusDot"
statusDot.Size = UDim2.new(0, 6, 0, 6)
statusDot.Position = UDim2.new(0, 0, 0, 5)
statusDot.BackgroundColor3 = THEME_YELLOW
statusDot.BorderSizePixel = 0
local dotCorner: UICorner = Instance.new("UICorner")
dotCorner.CornerRadius = UDim.new(1, 0)
dotCorner.Parent = statusDot
statusDot.Parent = statusPanel

local statusLine1: TextLabel = createLabel(statusPanel, "StatusLine1", "Disconnected", {
	position = UDim2.new(0, 12, 0, 0),
	size = UDim2.new(1, -12, 0, 15),
	color = THEME_TEXT,
	fontSize = 12,
	font = Enum.Font.GothamMedium,
})

local statusLine2: TextLabel = createLabel(statusPanel, "StatusLine2", "apply 0/s  ·  4ms  ·  q0", {
	position = UDim2.new(0, 0, 0, 22),
	size = UDim2.new(1, 0, 0, 14),
	color = THEME_TEXT_DIM,
	fontSize = 10,
	font = Enum.Font.RobotoMono,
})

local statusLine3: TextLabel = createLabel(statusPanel, "StatusLine3", "project dynamic  ·  fetch 0  ·  r0", {
	position = UDim2.new(0, 0, 0, 36),
	size = UDim2.new(1, 0, 0, 14),
	color = THEME_TEXT_DIM,
	fontSize = 9,
	font = Enum.Font.Gotham,
})

-- Status dot pulse tween (created once, controlled by status)
local statusPulseTween: Tween? = nil
local lastStatusLine1Text = ""
local lastStatusLine1Color: Color3? = nil
local lastStatusLine2Text = ""
local lastStatusLine3Text = ""
local lastStatusLine3Color: Color3? = nil
local lastRetryHistoryVisible: boolean? = nil
local lastTimeTravelDisplayKey = ""
local lastHistoryRowTexts: { string } = table.create(5, "")
local lastHistoryRowColors: { Color3? } = table.create(5)

-- ═══ Feature Toggles Panel ══════════════════════════════════════════════════

local togglesPanel: Frame = createPanel(mainFrame, "TogglesPanel", 2)

local togglesLayout: UIListLayout = Instance.new("UIListLayout")
togglesLayout.SortOrder = Enum.SortOrder.LayoutOrder
togglesLayout.Padding = UDim.new(0, 4)
togglesLayout.Parent = togglesPanel

local _, binaryModelsTrack, _ = createToggleSwitch(togglesPanel, "BinaryModelsToggle", "Binary Models", SETTINGS.binaryModels, 1)
local _, buildersTrack, _ = createToggleSwitch(togglesPanel, "BuildersToggle", "Builders", SETTINGS.buildersEnabled, 2)
local _, timeTravelTrack, _ = createToggleSwitch(togglesPanel, "TimeTravelToggle", "Time Travel", SETTINGS.timeTravelUI, 3)

-- ═══ Time-Travel Panel ══════════════════════════════════════════════════════

local timeTravelPanel: Frame = createPanel(mainFrame, "TimeTravelPanel", 3, 178)
timeTravelPanel.Visible = SETTINGS.timeTravelUI

-- Navigation row
local ttNavRow: Frame = Instance.new("Frame")
ttNavRow.Name = "NavRow"
ttNavRow.Size = UDim2.new(1, 0, 0, 24)
ttNavRow.BackgroundTransparency = 1
ttNavRow.Parent = timeTravelPanel

local ttNavLayout: UIListLayout = Instance.new("UIListLayout")
ttNavLayout.FillDirection = Enum.FillDirection.Horizontal
ttNavLayout.SortOrder = Enum.SortOrder.LayoutOrder
ttNavLayout.Padding = UDim.new(0, 4)
ttNavLayout.VerticalAlignment = Enum.VerticalAlignment.Center
ttNavLayout.Parent = ttNavRow

local btnJumpOldest: TextButton = createSmallButton(ttNavRow, "JumpOldest", "<<", 28)
btnJumpOldest.LayoutOrder = 1
local btnStepBack: TextButton = createSmallButton(ttNavRow, "StepBack", "<", 24)
btnStepBack.LayoutOrder = 2

local ttSeqLabel: TextLabel = createLabel(ttNavRow, "SeqLabel", "LIVE", {
	size = UDim2.new(0, 64, 0, 24),
	color = THEME_ACCENT,
	fontSize = 12,
	font = Enum.Font.GothamBold,
	xAlign = Enum.TextXAlignment.Center,
	layoutOrder = 3,
})

local btnStepFwd: TextButton = createSmallButton(ttNavRow, "StepFwd", ">", 24)
btnStepFwd.LayoutOrder = 4
local btnJumpLatest: TextButton = createSmallButton(ttNavRow, "JumpLatest", ">>", 28)
btnJumpLatest.LayoutOrder = 5

-- LIVE badge
local liveBadge: Frame = Instance.new("Frame")
liveBadge.Name = "LiveBadge"
liveBadge.Size = UDim2.new(0, 36, 0, 16)
liveBadge.BackgroundColor3 = THEME_GREEN
liveBadge.BorderSizePixel = 0
liveBadge.LayoutOrder = 6
local liveBadgeCorner: UICorner = Instance.new("UICorner")
liveBadgeCorner.CornerRadius = UDim.new(0, 8)
liveBadgeCorner.Parent = liveBadge
local liveBadgeLabel: TextLabel = Instance.new("TextLabel")
liveBadgeLabel.Name = "Label"
liveBadgeLabel.Text = "LIVE"
liveBadgeLabel.Size = UDim2.new(1, 0, 1, 0)
liveBadgeLabel.BackgroundTransparency = 1
liveBadgeLabel.TextColor3 = Color3.fromRGB(255, 255, 255)
liveBadgeLabel.TextSize = 9
liveBadgeLabel.Font = Enum.Font.GothamBold
liveBadgeLabel.Parent = liveBadge
liveBadge.Parent = ttNavRow

-- Scrubber
local scrubberContainer: Frame = Instance.new("Frame")
scrubberContainer.Name = "ScrubberContainer"
scrubberContainer.Size = UDim2.new(1, 0, 0, 16)
scrubberContainer.Position = UDim2.new(0, 0, 0, 30)
scrubberContainer.BackgroundTransparency = 1
scrubberContainer.Parent = timeTravelPanel

local scrubberTrack: Frame = Instance.new("Frame")
scrubberTrack.Name = "Track"
scrubberTrack.Size = UDim2.new(1, 0, 0, 4)
scrubberTrack.Position = UDim2.new(0, 0, 0.5, -2)
scrubberTrack.BackgroundColor3 = THEME_SURFACE_ELEVATED
scrubberTrack.BorderSizePixel = 0
local scrubberTrackCorner: UICorner = Instance.new("UICorner")
scrubberTrackCorner.CornerRadius = UDim.new(1, 0)
scrubberTrackCorner.Parent = scrubberTrack
scrubberTrack.Parent = scrubberContainer

local scrubberFill: Frame = Instance.new("Frame")
scrubberFill.Name = "Fill"
scrubberFill.Size = UDim2.new(1, 0, 1, 0)
scrubberFill.BackgroundColor3 = Color3.fromRGB(72, 148, 255)
scrubberFill.BorderSizePixel = 0
local fillCorner: UICorner = Instance.new("UICorner")
fillCorner.CornerRadius = UDim.new(1, 0)
fillCorner.Parent = scrubberFill
scrubberFill.Parent = scrubberTrack

-- Scrubber thumb shadow (behind thumb for depth)
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

-- Scrubber thumb
local scrubberThumb: Frame = Instance.new("Frame")
scrubberThumb.Name = "Thumb"
scrubberThumb.Size = UDim2.new(0, 14, 0, 14)
scrubberThumb.Position = UDim2.new(1, -7, 0.5, -7)
scrubberThumb.AnchorPoint = Vector2.new(0.5, 0.5)
scrubberThumb.BackgroundColor3 = THEME_ACCENT
scrubberThumb.BorderSizePixel = 0
local thumbCorner: UICorner = Instance.new("UICorner")
thumbCorner.CornerRadius = UDim.new(1, 0)
thumbCorner.Parent = scrubberThumb
scrubberThumb.Parent = scrubberContainer

-- History list (5 entries)
local historyListFrame: Frame = Instance.new("Frame")
historyListFrame.Name = "HistoryList"
historyListFrame.Size = UDim2.new(1, 0, 0, 110)
historyListFrame.Position = UDim2.new(0, 0, 0, 52)
historyListFrame.BackgroundTransparency = 1
historyListFrame.Parent = timeTravelPanel

local HISTORY_ROW_COUNT = 5
local historyRowFrames: { Frame } = table.create(HISTORY_ROW_COUNT)
local historyRowLabels: { TextLabel } = table.create(HISTORY_ROW_COUNT)
for i = 1, HISTORY_ROW_COUNT do
	local rowFrame: Frame = Instance.new("Frame")
	rowFrame.Name = "RowFrame" .. tostring(i)
	local baseRowColor = if i % 2 == 1 then THEME_SURFACE else THEME_BG
	local baseRowTransparency = if i % 2 == 1 then 0.6 else 1
	rowFrame.Size = UDim2.new(1, 0, 0, 20)
	rowFrame.Position = UDim2.new(0, 0, 0, (i - 1) * 22)
	rowFrame.BackgroundColor3 = baseRowColor
	rowFrame.BackgroundTransparency = baseRowTransparency
	rowFrame.BorderSizePixel = 0
	local rowCorner: UICorner = Instance.new("UICorner")
	rowCorner.CornerRadius = UDim.new(0, 3)
	rowCorner.Parent = rowFrame
	rowFrame.Parent = historyListFrame

	local rowLabel: TextLabel = createLabel(rowFrame, "Label", "", {
		size = UDim2.new(1, -4, 1, 0),
		position = UDim2.new(0, 4, 0, 0),
		color = THEME_TEXT_DIM,
		fontSize = 11,
		font = Enum.Font.RobotoMono,
	})
	rowLabel.RichText = true

	-- Hover highlight
	local rowBtn: TextButton = Instance.new("TextButton")
	rowBtn.Name = "HoverRegion"
	rowBtn.Size = UDim2.new(1, 0, 1, 0)
	rowBtn.BackgroundTransparency = 1
	rowBtn.Text = ""
	rowBtn.Parent = rowFrame
	rowBtn.MouseEnter:Connect(function()
		TweenService:Create(rowFrame, TWEEN_FAST, { BackgroundTransparency = 0, BackgroundColor3 = THEME_SURFACE_ELEVATED }):Play()
	end)
	rowBtn.MouseLeave:Connect(function()
		TweenService:Create(rowFrame, TWEEN_FAST, {
			BackgroundTransparency = baseRowTransparency,
			BackgroundColor3 = baseRowColor,
		}):Play()
	end)
	rowBtn.MouseButton1Click:Connect(function()
		if HISTORY.fetchFailed or HISTORY.busy then
			return
		end
		local entryCount: number = #HISTORY.entries
		local rowIdx: number = entryCount - (i - 1)
		if rowIdx < 1 or rowIdx > entryCount then
			return
		end
		TimeTravel.rewindToIndex(rowIdx)
	end)

	historyRowFrames[i] = rowFrame
	historyRowLabels[i] = rowLabel
end

-- Retry button for failed history fetch
local retryHistoryBtn: TextButton = createSmallButton(timeTravelPanel, "RetryHistory", "Retry", 52)
retryHistoryBtn.Position = UDim2.new(1, -52, 0, 0)
retryHistoryBtn.Visible = false

-- ═══ Settings Panel (collapsible) ═══════════════════════════════════════════

local settingsPanel: Frame = createPanel(mainFrame, "SettingsPanel", 4)
settingsPanel.Visible = false
settingsPanel.ClipsDescendants = true

local settingsHeaderRow: Frame = Instance.new("Frame")
settingsHeaderRow.Name = "HeaderRow"
settingsHeaderRow.Size = UDim2.new(1, 0, 0, 20)
settingsHeaderRow.BackgroundTransparency = 1
settingsHeaderRow.LayoutOrder = 0
settingsHeaderRow.Parent = settingsPanel

local settingsDisclosure: TextLabel = Instance.new("TextLabel")
settingsDisclosure.Name = "Disclosure"
settingsDisclosure.Text = "\226\150\184" -- ▸
settingsDisclosure.Size = UDim2.new(0, 12, 0, 20)
settingsDisclosure.BackgroundTransparency = 1
settingsDisclosure.TextColor3 = THEME_TEXT_DIM
settingsDisclosure.TextSize = 10
settingsDisclosure.Font = Enum.Font.GothamBold
settingsDisclosure.Parent = settingsHeaderRow

local settingsTitle: TextLabel = createLabel(settingsHeaderRow, "Title", "Settings", {
	position = UDim2.new(0, 16, 0, 0),
	size = UDim2.new(1, -16, 0, 20),
	color = THEME_TEXT,
	fontSize = 13,
	font = Enum.Font.GothamBold,
})

local settingsSubtitle: TextLabel = createLabel(settingsPanel, "Subtitle", "Sync is automatic. These settings are local and optional.", {
	color = THEME_TEXT_DIM,
	fontSize = 10,
	font = Enum.Font.Gotham,
	layoutOrder = 1,
	wrap = true,
})

local settingsLayout: UIListLayout = Instance.new("UIListLayout")
settingsLayout.SortOrder = Enum.SortOrder.LayoutOrder
settingsLayout.Padding = UDim.new(0, 4)
settingsLayout.Parent = settingsPanel

local historyBufferLabel: TextLabel = createLabel(settingsPanel, "HistoryBufferLabel", "Timeline depth", {
	color = THEME_TEXT_DIM,
	fontSize = 10,
	font = Enum.Font.Gotham,
	layoutOrder = 10,
})

local historyBufferRow: Frame = Instance.new("Frame")
historyBufferRow.Name = "HistoryBufferRow"
historyBufferRow.Size = UDim2.new(1, 0, 0, 24)
historyBufferRow.BackgroundTransparency = 1
historyBufferRow.LayoutOrder = 11
historyBufferRow.Parent = settingsPanel

local historyBufferLayout: UIListLayout = Instance.new("UIListLayout")
historyBufferLayout.FillDirection = Enum.FillDirection.Horizontal
historyBufferLayout.SortOrder = Enum.SortOrder.LayoutOrder
historyBufferLayout.Padding = UDim.new(0, 4)
historyBufferLayout.VerticalAlignment = Enum.VerticalAlignment.Center
historyBufferLayout.Parent = historyBufferRow

local historyBufferDecreaseBtn: TextButton = createSmallButton(historyBufferRow, "HistoryBufferDecrease", "-", 24)
historyBufferDecreaseBtn.LayoutOrder = 1

local historyBufferValueLabel: TextLabel = createLabel(historyBufferRow, "HistoryBufferValue", tostring(SETTINGS.historyBuffer), {
	size = UDim2.new(0, 52, 0, 24),
	color = THEME_TEXT,
	fontSize = 12,
	font = Enum.Font.RobotoMono,
	xAlign = Enum.TextXAlignment.Center,
	layoutOrder = 2,
})

local historyBufferIncreaseBtn: TextButton = createSmallButton(historyBufferRow, "HistoryBufferIncrease", "+", 24)
historyBufferIncreaseBtn.LayoutOrder = 3

local historyBufferRefreshBtn: TextButton = createSmallButton(historyBufferRow, "HistoryBufferRefresh", "Refresh", 64)
historyBufferRefreshBtn.LayoutOrder = 4

-- Settings disclosure row (placed inside toggles panel so it's not orphaned)
local settingsToggleRow: Frame = Instance.new("Frame")
settingsToggleRow.Name = "SettingsToggleRow"
settingsToggleRow.Size = UDim2.new(1, 0, 0, 24)
settingsToggleRow.BackgroundTransparency = 1
settingsToggleRow.LayoutOrder = 10
settingsToggleRow.Parent = togglesPanel

local settingsToggleBtn: TextButton = Instance.new("TextButton")
settingsToggleBtn.Name = "SettingsToggleBtn"
settingsToggleBtn.Text = ""
settingsToggleBtn.Size = UDim2.new(1, 0, 1, 0)
settingsToggleBtn.BackgroundTransparency = 1
settingsToggleBtn.AutoButtonColor = false
settingsToggleBtn.Parent = settingsToggleRow

local settingsToggleLabel: TextLabel = Instance.new("TextLabel")
settingsToggleLabel.Name = "Label"
settingsToggleLabel.Text = "\226\150\184 Settings" -- ▸ Settings
settingsToggleLabel.Size = UDim2.new(1, 0, 1, 0)
settingsToggleLabel.BackgroundTransparency = 1
settingsToggleLabel.TextColor3 = THEME_TEXT_DIM
settingsToggleLabel.TextSize = 12
settingsToggleLabel.Font = Enum.Font.RobotoMono
settingsToggleLabel.TextXAlignment = Enum.TextXAlignment.Left
settingsToggleLabel.Parent = settingsToggleRow

-- ─── UI Event Handlers ──────────────────────────────────────────────────────

local function updateHistoryBufferUI()
	historyBufferLabel.Text = "Timeline depth"
	historyBufferValueLabel.Text = tostring(SETTINGS.historyBuffer)
end

updateHistoryBufferUI()

-- Toggle switches
local function getTrackClickRegion(track: Frame): TextButton?
	return track:FindFirstChild("ClickRegion") :: TextButton?
end

local binaryModelsClickRegion = getTrackClickRegion(binaryModelsTrack)
if binaryModelsClickRegion then
	binaryModelsClickRegion.MouseButton1Click:Connect(function()
		SETTINGS.binaryModels = not SETTINGS.binaryModels
		animateToggle(binaryModelsTrack, SETTINGS.binaryModels)
		saveSetting("VertigoSyncBinaryModels", SETTINGS.binaryModels)
		Workspace:SetAttribute("VertigoSyncBinaryModels", SETTINGS.binaryModels)
	end)
end

local buildersClickRegion = getTrackClickRegion(buildersTrack)
if buildersClickRegion then
	buildersClickRegion.MouseButton1Click:Connect(function()
		SETTINGS.buildersEnabled = not SETTINGS.buildersEnabled
		animateToggle(buildersTrack, SETTINGS.buildersEnabled)
		BUILDERS.enabled = SETTINGS.buildersEnabled and isEditMode()
		saveSetting("VertigoSyncBuildersEnabled", SETTINGS.buildersEnabled)
		Workspace:SetAttribute("VertigoSyncBuildersEnabled", BUILDERS.enabled)
	end)
end

local timeTravelClickRegion = getTrackClickRegion(timeTravelTrack)
if timeTravelClickRegion then
	timeTravelClickRegion.MouseButton1Click:Connect(function()
		SETTINGS.timeTravelUI = not SETTINGS.timeTravelUI
		animateToggle(timeTravelTrack, SETTINGS.timeTravelUI)
		timeTravelPanel.Visible = SETTINGS.timeTravelUI
		saveSetting("VertigoSyncTimeTravelUI", SETTINGS.timeTravelUI)
	end)
end

-- Settings disclosure toggle (inline in toggles panel)
settingsToggleBtn.MouseButton1Click:Connect(function()
	settingsPanel.Visible = not settingsPanel.Visible
	-- Swap disclosure triangle character (instant, no tween)
	local arrow: string = if settingsPanel.Visible then "\226\150\190 Settings" else "\226\150\184 Settings" -- ▾ or ▸
	settingsToggleLabel.Text = arrow
	settingsDisclosure.Text = if settingsPanel.Visible then "\226\150\190" else "\226\150\184"
end)

historyBufferDecreaseBtn.MouseButton1Click:Connect(function()
	local nextValue = math.max(16, SETTINGS.historyBuffer - 16)
	if nextValue == SETTINGS.historyBuffer then
		return
	end
	SETTINGS.historyBuffer = nextValue
	saveSetting("VertigoSyncHistoryBuffer", SETTINGS.historyBuffer)
	updateHistoryBufferUI()
end)

historyBufferIncreaseBtn.MouseButton1Click:Connect(function()
	local nextValue = math.min(1024, SETTINGS.historyBuffer + 16)
	if nextValue == SETTINGS.historyBuffer then
		return
	end
	SETTINGS.historyBuffer = nextValue
	saveSetting("VertigoSyncHistoryBuffer", SETTINGS.historyBuffer)
	updateHistoryBufferUI()
end)

historyBufferRefreshBtn.MouseButton1Click:Connect(function()
	HISTORY.fetchFailed = false
	TimeTravel.fetchHistory(true)
end)

-- Time-travel button handlers
btnJumpOldest.MouseButton1Click:Connect(function()
	if HISTORY.fetchFailed then
		return
	end
	if not HISTORY.loaded then
		TimeTravel.fetchHistory(true)
	end
	TimeTravel.jumpToOldest()
end)

btnStepBack.MouseButton1Click:Connect(function()
	if HISTORY.fetchFailed then
		return
	end
	if not HISTORY.loaded then
		TimeTravel.fetchHistory(true)
	end
	TimeTravel.stepBackward()
end)

btnStepFwd.MouseButton1Click:Connect(function()
	TimeTravel.stepForward()
end)

btnJumpLatest.MouseButton1Click:Connect(function()
	TimeTravel.jumpToLatest()
end)

-- Retry history button
retryHistoryBtn.MouseButton1Click:Connect(function()
	HISTORY.fetchFailed = false
	retryHistoryBtn.Visible = false
	TimeTravel.fetchHistory(true)
end)

-- Welcome screen: "Check Connection" triggers immediate health check
welcomeCheckBtn.MouseButton1Click:Connect(function()
	connectionState = "connecting"
	local healthOk = checkHealth()
	if healthOk then
		hasEverConnected = true
		connectionState = "connected"
		-- Fade out welcome screen
		TweenService:Create(welcomeFrame, TWEEN_SLOW, { BackgroundTransparency = 1 }):Play()
		task.delay(0.3, function()
			welcomeFrame.Visible = false
			welcomeFrame.BackgroundTransparency = 0
		end)
		resyncRequested = true
	else
		connectionState = "error"
		showToast(string.format("Server not reachable at %s", getServerBaseUrl()), TOAST_COLOR_ERROR)
	end
end)

-- Welcome screen: "Learn more" opens documentation URL
welcomeLearnMore.MouseButton1Click:Connect(function()
	-- Cannot open URL from plugin; set attribute for external tooling
	Workspace:SetAttribute("VertigoSyncDocsRequest", "https://github.com/vertigo-sync/vertigo-sync")
	showToast("Docs: github.com/vertigo-sync", TOAST_COLOR_INFO)
end)

-- Workspace attribute toggles for external control
Workspace:GetAttributeChangedSignal("VertigoSyncBinaryModels"):Connect(function()
	local val: any = Workspace:GetAttribute("VertigoSyncBinaryModels")
	if type(val) == "boolean" then
		SETTINGS.binaryModels = val
		animateToggle(binaryModelsTrack, SETTINGS.binaryModels)
	end
end)

Workspace:GetAttributeChangedSignal("VertigoSyncBuildersEnabled"):Connect(function()
	local val: any = Workspace:GetAttribute("VertigoSyncBuildersEnabled")
	if type(val) == "boolean" then
		SETTINGS.buildersEnabled = val
		BUILDERS.enabled = SETTINGS.buildersEnabled and isEditMode()
		animateToggle(buildersTrack, SETTINGS.buildersEnabled)
	end
end)

-- ─── UI Status Refresh ──────────────────────────────────────────────────────

local lastStatusForPulse: SyncStatus = "disconnected"
local lastConnectionStateForUI: ConnectionState = "waiting"

local function refreshStatusUI()
	-- ─── Connection state machine → visual state mapping ─────────────────
	-- Update connectionState based on currentStatus and reconnect info
	if currentStatus == "connected" then
		if not hasEverConnected then
			hasEverConnected = true
			-- Fade out welcome screen on first successful connection
			if welcomeFrame.Visible then
				TweenService:Create(welcomeFrame, TWEEN_SLOW, { BackgroundTransparency = 1 }):Play()
				task.delay(0.3, function()
					welcomeFrame.Visible = false
					welcomeFrame.BackgroundTransparency = 0
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
	welcomeFrame.Visible = not hasEverConnected and connectionState ~= "connected"

	-- Status line 1: connection indicator with dot
	local statusText: string
	local dotColor: Color3
	local line1Color: Color3 = THEME_TEXT
	if connectionState == "connected" then
		statusText = "Connected"
		dotColor = THEME_GREEN
	elseif connectionState == "reconnecting" then
		statusText = string.format("Reconnecting %d", connectionReconnectAttempt)
		dotColor = THEME_YELLOW
	elseif connectionState == "connecting" then
		statusText = "Connecting..."
		dotColor = THEME_ACCENT
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
		dotColor = THEME_RED
		line1Color = THEME_RED
	else -- "waiting"
		statusText = "Waiting for server"
		dotColor = THEME_ACCENT
	end

	local hashShort: string = if lastHash ~= nil then string.sub(lastHash, 1, 8) else "--------"
	local transportLabel: string = if transportMode == "ws" then "ws" elseif transportMode == "poll" then "poll" else "idle"
	local nextStatusLine1Text = string.format("%s  ·  %s  ·  %s", statusText, transportLabel, hashShort)
	if lastStatusLine1Text ~= nextStatusLine1Text then
		lastStatusLine1Text = nextStatusLine1Text
		statusLine1.Text = nextStatusLine1Text
	end
	if lastStatusLine1Color ~= line1Color then
		lastStatusLine1Color = line1Color
		statusLine1.TextColor3 = line1Color
	end
	if statusDot.BackgroundColor3 ~= dotColor then
		TweenService:Create(statusDot, TWEEN_SLOW, { BackgroundColor3 = dotColor }):Play()
	end

	-- Manage pulse tween based on connection state
	if connectionState ~= lastConnectionStateForUI then
		lastConnectionStateForUI = connectionState
		if statusPulseTween then
			statusPulseTween:Cancel()
			statusPulseTween = nil
		end
		if connectionState == "connecting" or connectionState == "reconnecting" then
			statusDot.BackgroundTransparency = 0.2
			statusPulseTween = TweenService:Create(statusDot, TWEEN_PULSE, { BackgroundTransparency = 0.55 })
			statusPulseTween:Play()
		elseif connectionState == "waiting" then
			statusDot.BackgroundTransparency = 0.35
		elseif connectionState == "connected" then
			statusDot.BackgroundTransparency = 0
		else
			statusDot.BackgroundTransparency = 0
		end
	end

	-- Status line 2: throughput metrics
	local budgetMs: number = math.floor(adaptiveApplyBudgetSeconds * 1000 + 0.5)
	local queueDepth: number = math.max(#pendingQueue - pendingQueueHead + 1, 0)
	local nextStatusLine2Text = string.format("apply %d/s  ·  %dms  ·  q%d", appliedPerSecond, budgetMs, queueDepth)
	if lastStatusLine2Text ~= nextStatusLine2Text then
		lastStatusLine2Text = nextStatusLine2Text
		statusLine2.Text = nextStatusLine2Text
	end

	-- Status line 3: project bootstrap status + reconnects
	local fetchDepth: number = math.max(#fetchQueue - fetchQueueHead + 1, 0)
	local nextStatusLine3Text = string.format("%s  ·  fetch %d  ·  r%d", projectStatusLabel(), fetchDepth, reconnectCount)
	if lastStatusLine3Text ~= nextStatusLine3Text then
		lastStatusLine3Text = nextStatusLine3Text
		statusLine3.Text = nextStatusLine3Text
	end
	local nextStatusLine3Color: Color3
	if PROJECT.mode == "mismatch" then
		nextStatusLine3Color = THEME_RED
	elseif PROJECT.mode == "legacy" then
		nextStatusLine3Color = THEME_YELLOW
	else
		nextStatusLine3Color = THEME_TEXT_DIM
	end
	if lastStatusLine3Color ~= nextStatusLine3Color then
		lastStatusLine3Color = nextStatusLine3Color
		statusLine3.TextColor3 = nextStatusLine3Color
	end

	-- Time-travel panel
	if SETTINGS.timeTravelUI then
		local nextDisplayKey: string
		if HISTORY.active and HISTORY.currentIndex > 0 and #HISTORY.entries > 0 then
			local ratio: number = HISTORY.currentIndex / math.max(#HISTORY.entries, 1)
			nextDisplayKey = string.format("tt:%d:%d", HISTORY.currentIndex, #HISTORY.entries)
			if lastTimeTravelDisplayKey ~= nextDisplayKey then
				lastTimeTravelDisplayKey = nextDisplayKey
				ttSeqLabel.Text = string.format("%d / %d", HISTORY.currentIndex, #HISTORY.entries)
				ttSeqLabel.TextColor3 = THEME_ACCENT
				local thumbXScale = ratio
				TweenService:Create(scrubberFill, TWEEN_FAST, { Size = UDim2.new(ratio, 0, 1, 0) }):Play()
				TweenService:Create(scrubberThumb, TWEEN_FAST, { Position = UDim2.new(thumbXScale, 0, 0.5, 0) }):Play()
				TweenService:Create(scrubberThumbShadow, TWEEN_FAST, { Position = UDim2.new(thumbXScale, 1, 0.5, 1) }):Play()
				liveBadge.BackgroundColor3 = THEME_SURFACE_ELEVATED
				liveBadgeLabel.TextColor3 = THEME_TEXT_DIM
			end
		else
			nextDisplayKey = "live"
			if lastTimeTravelDisplayKey ~= nextDisplayKey then
				lastTimeTravelDisplayKey = nextDisplayKey
				ttSeqLabel.Text = "LIVE"
				ttSeqLabel.TextColor3 = THEME_GREEN
				TweenService:Create(scrubberFill, TWEEN_FAST, { Size = UDim2.new(1, 0, 1, 0) }):Play()
				TweenService:Create(scrubberThumb, TWEEN_FAST, { Position = UDim2.new(1, 0, 0.5, 0) }):Play()
				TweenService:Create(scrubberThumbShadow, TWEEN_FAST, { Position = UDim2.new(1, 1, 0.5, 1) }):Play()
				liveBadge.BackgroundColor3 = THEME_GREEN
				liveBadgeLabel.TextColor3 = Color3.fromRGB(255, 255, 255)
			end
		end

		-- Show retry button if fetch failed
		if lastRetryHistoryVisible ~= HISTORY.fetchFailed then
			lastRetryHistoryVisible = HISTORY.fetchFailed
			retryHistoryBtn.Visible = HISTORY.fetchFailed
		end

		-- Update history rows
		local entryCount: number = #HISTORY.entries
		for i = 1, HISTORY_ROW_COUNT do
			local rowIdx: number = entryCount - (i - 1)
			if rowIdx >= 1 and rowIdx <= entryCount then
				local entry: HistoryEntry = HISTORY.entries[rowIdx]
				local timeStr: string = if type(entry.timestamp) == "string" then string.sub(entry.timestamp, 12, 19) else "??:??:??"
				local nextRowText = string.format(
					'%s  <font color="#34C759">+%d</font> <font color="#FF9F0A">~%d</font> <font color="#FF453A">-%d</font>',
					timeStr,
					entry.added,
					entry.modified,
					entry.deleted
				)
				local nextRowColor = if rowIdx == HISTORY.currentIndex then THEME_ACCENT else THEME_TEXT_DIM
				if lastHistoryRowTexts[i] ~= nextRowText then
					lastHistoryRowTexts[i] = nextRowText
					historyRowLabels[i].Text = nextRowText
				end
				if lastHistoryRowColors[i] ~= nextRowColor then
					lastHistoryRowColors[i] = nextRowColor
					historyRowLabels[i].TextColor3 = nextRowColor
				end
			else
				if lastHistoryRowTexts[i] ~= "" then
					lastHistoryRowTexts[i] = ""
					historyRowLabels[i].Text = ""
				end
				if lastHistoryRowColors[i] ~= THEME_TEXT_DIM then
					lastHistoryRowColors[i] = THEME_TEXT_DIM
					historyRowLabels[i].TextColor3 = THEME_TEXT_DIM
				end
			end
		end
	else
		lastTimeTravelDisplayKey = ""
	end
end


-- ─── Toolbar UI ─────────────────────────────────────────────────────────────

local toolbar = plugin:CreateToolbar("VERTIGO SYNC")
local syncButton = toolbar:CreateButton(
	"Toggle Sync",
	"Toggle Vertigo Sync realtime synchronization",
	"rbxassetid://4458901886"
)
local resyncButton = toolbar:CreateButton(
	"Resync",
	"Force full snapshot reconciliation",
	"rbxassetid://4458902530"
)
local widgetToggleButton = toolbar:CreateButton(
	"Panel",
	"Toggle Vertigo Sync panel",
	"rbxassetid://4458901886"
)

local function updateButtonAppearance()
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
		closeWebSocket("disabled")
		setStatusAttributes("disconnected", lastHash)
	end
	updateButtonAppearance()
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
	widget.Enabled = not widget.Enabled
	-- If opening the panel while disconnected and not first-time, show a helpful toast
	if widget.Enabled and currentStatus ~= "connected" and hasEverConnected then
		showToast("Server not running — start with: vertigo-sync serve --turbo", TOAST_COLOR_INFO)
	end
end)

-- ─── Runtime loops ──────────────────────────────────────────────────────────

@native
local function tickSyncManager()
	if not syncEnabled then
		transportMode = "idle"
		closeWebSocket("disabled")
		setStatusAttributes("disconnected", lastHash)
		updateButtonAppearance()
		return
	end

	if not isEditMode() then
		transportMode = "idle"
		closeWebSocket("not_edit_mode")
		setStatusAttributes("disconnected", lastHash)
		updateButtonAppearance()
		return
	end

	local now = os.clock()
	if now - lastHealthCheckAt >= HEALTH_POLL_SECONDS then
		lastHealthCheckAt = now
		if not checkHealth() then
			closeWebSocket("health_failed")
			setStatusAttributes("disconnected", lastHash)
			updateButtonAppearance()
			return
		end
	end

	if resyncRequested or lastHash == nil then
		if not ensureProjectBootstrap(false) then
			closeWebSocket("project_bootstrap_pending")
			setStatusAttributes(if PROJECT.blocked then "error" else "disconnected", lastHash)
			updateButtonAppearance()
			return
		end
		local synced = syncFromSnapshot(resyncRequested and "requested" or "bootstrap")
		updateButtonAppearance()
		if not synced then
			return
		end
	end

	local wsReady = tryConnectWebSocket()
	if wsReady and wsConnected then
		transportMode = "ws"
		updateButtonAppearance()
		return
	end

	transportMode = "poll"
	if now >= nextPollAt then
		pollDiff()
		updateButtonAppearance()
		nextPollAt = now + pollInterval
		pollInterval = math.min(POLL_INTERVAL_MAX, pollInterval * 1.15)
	end
end

-- ─── Initialization ──────────────────────────────────────────────────────────

BUILDERS.enabled = SETTINGS.buildersEnabled and isEditMode()
initInstancePool()
Workspace:SetAttribute("VertigoSyncServerUrl", getServerBaseUrl())

Workspace:SetAttribute("VertigoSyncPluginVersion", PLUGIN_VERSION)
Workspace:SetAttribute("VertigoSyncRealtimeDefault", true)
Workspace:SetAttribute("VertigoSyncBinaryModels", SETTINGS.binaryModels)
Workspace:SetAttribute("VertigoSyncBuildersEnabled", BUILDERS.enabled)
setProjectStatus("bootstrapping", "Waiting for /project", nil, false)
setStatusAttributes("disconnected", nil)
bootstrapManagedIndex()
attachActivePathGuards()
info(string.format(
	"Plugin initialized. version=%s mode=%s ws=%s binaryModels=%s builders=%s",
	PLUGIN_VERSION,
	describeStudioMode(),
	if WebSocketService ~= nil then "available" else "unavailable",
	tostring(SETTINGS.binaryModels),
	tostring(BUILDERS.enabled)
))
updateButtonAppearance()
flushMetrics(true)

task.defer(function()
	ensureProjectBootstrap(false)
end)

-- ─── Heartbeat Loop ──────────────────────────────────────────────────────────

RunService.Heartbeat:Connect(function()
	-- When time-travel is active, skip polling/WS but still drain the apply queue
	if not HISTORY.active then
		processFetchQueue()
	end
	processApplyQueue()
	flushMetrics(false)
end)

-- ─── Sync Manager Loop ──────────────────────────────────────────────────────

task.spawn(function()
	while true do
		if not HISTORY.active then
			tickSyncManager()
		end
		task.wait(0.25)
	end
end)

-- ─── UI Refresh Loop (0.5s timer, NOT Heartbeat) ────────────────────────────

task.spawn(function()
	while true do
		refreshStatusUI()
		task.wait(UI_STATUS_REFRESH_SECONDS)
	end
end)

-- ─── History Background Fetch ────────────────────────────────────────────────

task.spawn(function()
	-- Wait for initial sync to complete before fetching history
	task.wait(3)
	while true do
		if SETTINGS.timeTravelUI and currentStatus == "connected" and not HISTORY.active then
			TimeTravel.fetchHistory()
		end
		task.wait(HISTORY_REFRESH_INTERVAL_SECONDS)
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
	-- Wait for initial connection before first report
	task.wait(STATE_REPORT_INTERVAL_SECONDS)
	while true do
		if currentStatus == "connected" then
			reportPluginState()
		end
		task.wait(STATE_REPORT_INTERVAL_SECONDS)
	end
end)

-- ─── Managed Index Reporting Loop (30s timer) ────────────────────────────────

task.spawn(function()
	-- Wait for initial sync to settle
	task.wait(MANAGED_REPORT_INTERVAL_SECONDS)
	while true do
		if currentStatus == "connected" then
			reportPluginManaged()
		end
		task.wait(MANAGED_REPORT_INTERVAL_SECONDS)
	end
end)


end -- _initPlugin
_initPlugin()
Workspace:GetAttributeChangedSignal("VertigoSyncServerUrl"):Connect(handleServerUrlChanged)
