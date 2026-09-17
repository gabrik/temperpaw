// dark-factory Cedar policies
//
// FactoryTasks are driven by the factory WASM modules (factory_planner,
// factory_implementer, factory_validator, factory_publisher, factory_janitor)
// which run as the system service; the two human gates (plan approval, merge
// approval) are Admin-only in v1 — a scoped factory-operator principal kind
// is future work (see ADR-0066 D8 notes).
//
// Action ids are listed in both namespaced (Entity.Action) and bare forms
// until the deployed engine's naming is pinned down by live dispatch, same
// as paw-compute/policies/compute.cedar.

// Admins can do anything (includes the human gates and manual recovery).
permit(
    principal is Admin,
    action,
    resource is FactoryTask
);

permit(
    principal is Admin,
    action,
    resource is FactoryConfig
);

// ADR-0069: FactoryRepo profiles are operator-managed.
permit(
    principal is Admin,
    action,
    resource is FactoryRepo
);

// Read/list are open within the tenant (API access is already
// credential-gated); the console and the factory modules both read rows.
permit(
    principal,
    action in [
        Action::"read",
        Action::"list"
    ],
    resource is FactoryTask
);

permit(
    principal,
    action in [
        Action::"read",
        Action::"list"
    ],
    resource is FactoryConfig
);

// Read/list FactoryRepos is open within the tenant (console selector +
// modules reading profiles); writes are Admin-only below.
permit(
    principal,
    action in [
        Action::"read",
        Action::"list"
    ],
    resource is FactoryRepo
);

// ADR-0068: the console subscribes to the tenant-scoped entity event feed
// (GET /tdata/$events) for live activity. read_events is gated on resource
// Entity; the feed itself is tenant-scoped by the server, so any
// authenticated principal on this tenant may subscribe (mirrors DEN).
permit(
    principal,
    action == Action::"read_events",
    resource
);

// Task creation and the external kickoff: any Agent on this tenant may
// request a task and start planning.
permit(
    principal is Agent,
    action in [
        Action::"create",
        Action::"FactoryTask.StartPlanning", Action::"StartPlanning"
    ],
    resource is FactoryTask
);

// Human decision gates: Admin only (the dark-factory console).
permit(
    principal is Admin,
    action in [
        Action::"FactoryTask.ApprovePlan", Action::"ApprovePlan",
        Action::"FactoryTask.RejectPlan", Action::"RejectPlan",
        Action::"FactoryTask.ApproveMerge", Action::"ApproveMerge",
        Action::"FactoryTask.RequestChanges", Action::"RequestChanges"
    ],
    resource is FactoryTask
);

// Module-reported transitions: dispatched by the factory WASM modules
// running as the system service.
permit(
    principal,
    action in [
        Action::"FactoryTask.SubmitPlan", Action::"SubmitPlan",
        Action::"FactoryTask.PinRepoProfile", Action::"PinRepoProfile",
        Action::"FactoryTask.SubmitImplementation", Action::"SubmitImplementation",
        Action::"FactoryTask.ValidationPassed", Action::"ValidationPassed",
        Action::"FactoryTask.ValidationFailed", Action::"ValidationFailed",
        Action::"FactoryTask.PublishPullRequest", Action::"PublishPullRequest",
        Action::"FactoryTask.RecordDeployed", Action::"RecordDeployed",
        Action::"FactoryTask.ObservationPassed", Action::"ObservationPassed",
        Action::"FactoryTask.ObservationFailed", Action::"ObservationFailed",
        Action::"FactoryTask.MergeFinalized", Action::"MergeFinalized",
        Action::"FactoryTask.FailTask", Action::"FailTask"
    ],
    resource is FactoryTask
) when {
    principal.agent_type == "system"
};

// Re-check ticks and the hard-stop: dispatched by the state_timeout scheduler
// (or by an Admin nudging a stuck task manually).
permit(
    principal,
    action in [
        Action::"FactoryTask.CheckPlanning", Action::"CheckPlanning",
        Action::"FactoryTask.CheckImplementing", Action::"CheckImplementing",
        Action::"FactoryTask.CheckValidating", Action::"CheckValidating",
        Action::"FactoryTask.CheckPublishing", Action::"CheckPublishing",
        Action::"FactoryTask.CheckDeploying", Action::"CheckDeploying",
        Action::"FactoryTask.CheckObserving", Action::"CheckObserving",
        Action::"FactoryTask.CheckFinalizing", Action::"CheckFinalizing",
        Action::"FactoryTask.ExpireTask", Action::"ExpireTask"
    ],
    resource is FactoryTask
) when {
    principal.agent_type == "system"
};

permit(
    principal is Admin,
    action in [
        Action::"FactoryTask.CheckPlanning", Action::"CheckPlanning",
        Action::"FactoryTask.CheckImplementing", Action::"CheckImplementing",
        Action::"FactoryTask.CheckValidating", Action::"CheckValidating",
        Action::"FactoryTask.CheckPublishing", Action::"CheckPublishing",
        Action::"FactoryTask.CheckDeploying", Action::"CheckDeploying",
        Action::"FactoryTask.CheckObserving", Action::"CheckObserving",
        Action::"FactoryTask.CheckFinalizing", Action::"CheckFinalizing",
        Action::"FactoryTask.ExpireTask", Action::"ExpireTask"
    ],
    resource is FactoryTask
);

// FactoryRepo rows are operator-managed: create + lifecycle actions are
// Admin-only (ADR-0069).
permit(
    principal is Admin,
    action in [
        Action::"create",
        Action::"FactoryRepo.Revise", Action::"Revise",
        Action::"FactoryRepo.Update", Action::"Update",
        Action::"FactoryRepo.Activate", Action::"Activate",
        Action::"FactoryRepo.Archive", Action::"Archive"
    ],
    resource is FactoryRepo
);

// FactoryArtifact rows (ADR-0067 D8): read/list are open within the tenant
// (the console diff pane); create requires an Agent principal — the factory
// publisher records patch artifacts over the loopback. Actionless entity.
permit(
    principal is Admin,
    action,
    resource is FactoryArtifact
);

permit(
    principal,
    action in [
        Action::"read",
        Action::"list"
    ],
    resource is FactoryArtifact
);

permit(
    principal is Agent,
    action == Action::"create",
    resource is FactoryArtifact
);

// FactoryConfig rows are operator-managed.
permit(
    principal is Admin,
    action in [
        Action::"create",
        Action::"FactoryConfig.Update", Action::"Update",
        Action::"FactoryConfig.Archive", Action::"Archive"
    ],
    resource is FactoryConfig
);

// Allow the factory modules to make HTTP calls: Temper loopback (entity
// rows, governed Execs), the sandbox provider data plane, and the GitHub
// REST API (checkout + publish — the GitHub token never leaves the host,
// ADR-0066 D7).
permit(
    principal is Agent,
    action == Action::"http_call",
    resource is HttpEndpoint
) when {
    ["factory_planner", "factory_implementer", "factory_validator", "factory_publisher", "factory_deployer", "factory_janitor"].contains(context.module)
};

// Allow the factory modules to access secrets (temper_api_url, sandbox
// provider keys, github_token, anthropic model credential).
permit(
    principal is Agent,
    action == Action::"access_secret",
    resource is Secret
) when {
    ["factory_planner", "factory_implementer", "factory_validator", "factory_publisher", "factory_deployer", "factory_janitor"].contains(context.module)
};
