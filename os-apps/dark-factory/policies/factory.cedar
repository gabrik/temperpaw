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
        Action::"FactoryTask.SubmitImplementation", Action::"SubmitImplementation",
        Action::"FactoryTask.ValidationPassed", Action::"ValidationPassed",
        Action::"FactoryTask.ValidationFailed", Action::"ValidationFailed",
        Action::"FactoryTask.PublishPullRequest", Action::"PublishPullRequest",
        Action::"FactoryTask.RecordMerged", Action::"RecordMerged",
        Action::"FactoryTask.ObservationPassed", Action::"ObservationPassed",
        Action::"FactoryTask.ObservationFailed", Action::"ObservationFailed",
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
        Action::"FactoryTask.CheckMerging", Action::"CheckMerging",
        Action::"FactoryTask.CheckObserving", Action::"CheckObserving",
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
        Action::"FactoryTask.CheckMerging", Action::"CheckMerging",
        Action::"FactoryTask.CheckObserving", Action::"CheckObserving",
        Action::"FactoryTask.ExpireTask", Action::"ExpireTask"
    ],
    resource is FactoryTask
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
    ["factory_planner", "factory_implementer", "factory_validator", "factory_publisher", "factory_janitor"].contains(context.module)
};

// Allow the factory modules to access secrets (temper_api_url, sandbox
// provider keys, github_token, anthropic model credential).
permit(
    principal is Agent,
    action == Action::"access_secret",
    resource is Secret
) when {
    ["factory_planner", "factory_implementer", "factory_validator", "factory_publisher", "factory_janitor"].contains(context.module)
};
