//! Tools owns request choices; Connections owns only the account that reaches
//! them. The same renderer/editor and acknowledged command path serve both.

use super::*;

pub(super) fn account_api<'a>(app: &'a App, name: &'a str) -> Option<&'a str> {
    service_of(app, name)
        .and_then(|s| s["api"].as_str())
        .or_else(|| {
            provider_list(app)
                .into_iter()
                .find(|p| p["name"].as_str() == Some(name))
                .and_then(|p| {
                    p["api"]
                        .as_str()
                        .filter(|api| adapter_of(app, api).is_some())
                })
        })
        .or_else(|| adapter_of(app, name).map(|_| name))
}

pub(super) fn access_rows(app: &App, name: &str, with_model: bool) -> Vec<Row> {
    let Some(s) = service_of(app, name) else {
        return Vec::new();
    };
    let Some(a) = adapter_of(app, s["api"].as_str().unwrap_or(name)) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if with_model && !a["shares_model_base"].as_bool().unwrap_or(false) {
        out.push(
            Row::new(
                Kind::Edit(Field::ServiceUrl(name.into(), Service::Search)),
                "Web API URL",
                s["base_url"].as_str().unwrap_or("").into(),
            )
            .help("The address of this account's web API. Provider choices belong to Tools."),
        );
    }
    if with_model && a["split"].as_bool().unwrap_or(false) {
        out.push(
            Row::new(
                Kind::Edit(Field::ServiceUrl(name.into(), Service::Fetch)),
                "Fetch URL",
                s["fetch_url"].as_str().unwrap_or("").into(),
            )
            .help("The address at which this account reads pages."),
        );
    }
    out
}

fn pinned(app: &App, service: Service) -> Option<&str> {
    runeengine::web::pin_key(service)
        .split('.')
        .fold(&app.settings["configuration"], |v, k| &v[k])
        .as_str()
}

fn chosen(app: &App, service: Service, target: &str) -> bool {
    if let Some(pin) = pinned(app, service) {
        return pin == target;
    }
    let now = serving(app, service);
    let parsed = runeengine::settings::services::Target::parse(target).ok();
    parsed.is_some_and(|t| match t.route {
        Some(route) => now == format!("{} ({route})", t.name),
        None => now == t.name,
    })
}

fn provider_value(app: &App, service: Service, target: &str) -> String {
    match (chosen(app, service, target), pinned(app, service).is_some()) {
        (true, true) => "Chosen".into(),
        (true, false) => "In use · default".into(),
        (false, _) => "Use ↵ · Del removes".into(),
    }
}

pub(super) fn provider_rows(app: &App, service: Service) -> Vec<Row> {
    let mut out = vec![Row::new(Kind::Fact, "PROVIDERS",
        "One provider serves this tool. Adding keeps it; Enter chooses it after a real request.".into()),
        Row::new(Kind::WebSelect(service, None), pin_default(service),
            if pinned(app, service).is_none() { "Chosen" } else { "Use ↵" }.into())
            .help("Use the default without choosing a third-party page reader.")];
    for name in connection_names(app) {
        let Some(s) = service_of(app, &name) else {
            continue;
        };
        let Some(a) = adapter_of(app, s["api"].as_str().unwrap_or(&name)) else {
            continue;
        };
        if !a[service_name(service)].as_bool().unwrap_or(false) {
            continue;
        }
        if a["routes"].as_bool().unwrap_or(false) {
            for route in kept_routes(app, &name, service) {
                let target = format!("{name}:{route}");
                out.push(Row::new(Kind::RouteUse(name.clone(), service, route), &target,
                    provider_value(app, service, &target))
                    .help("Enter proves and chooses this exact provider. Delete removes it from this tool only; the account stays."));
            }
        } else if !s[service_name(service)].is_null() {
            out.push(Row::new(Kind::WebSelect(service, Some(name.clone())), &name,
                provider_value(app, service, &name))
                .help("Enter proves and chooses this provider. Delete removes it from this tool only; the account stays."));
        }
        if a["zone"].as_bool().unwrap_or(false) && !s[service_name(service)].is_null() {
            out.push(Row::new(
                Kind::Edit(Field::ServiceZone(name.clone())),
                &format!("{name} SERP zone"),
                s["zone"].as_str().unwrap_or("").into(),
            ));
        }
    }
    if let Some(pin) = pinned(app, service) {
        if !out.iter().any(|r| r.label == pin) {
            out.push(Row::new(
                Kind::Fact,
                pin,
                "Unavailable · not saved for this tool".into(),
            ));
        }
    }
    out.push(Row::new(Kind::Navigate(Page::AddWebProvider { service }),
        "Add provider", "Add →".into())
        .help("Choose a connected account, then type a gateway route in an empty field. No sample or catalog route is added."));
    out
}

pub(super) fn add_rows(app: &App, service: Service) -> Vec<Row> {
    let mut out = vec![Row::new(
        Kind::Fact,
        "CONNECTED ACCOUNTS",
        "Choose the account to reach this provider. Endpoint and keys are managed in Connections."
            .into(),
    )];
    for name in connection_names(app) {
        let Some(api) = account_api(app, &name) else {
            continue;
        };
        let Some(a) = adapter_of(app, api) else {
            continue;
        };
        if !a[service_name(service)].as_bool().unwrap_or(false) {
            continue;
        }
        if a["routes"].as_bool().unwrap_or(false) {
            out.push(Row::new(Kind::RouteAdd(name.clone(), service), &name, "Add route ↵".into())
                .help("Opens an empty route field. The provider is kept only after one real request succeeds."));
        } else if service_of(app, &name).is_none_or(|s| s[service_name(service)].is_null()) {
            out.push(
                Row::new(
                    Kind::ServiceOn(name.clone(), service),
                    &name,
                    "Add ↵".into(),
                )
                .help("One real request must succeed before this account is listed as a provider."),
            );
        }
    }
    if out.len() == 1 {
        out.push(Row::new(
            Kind::Fact,
            "No account available",
            "Add its endpoint and key in Connections first.".into(),
        ));
    }
    out
}

pub(super) fn select(app: &mut App, d: &mut Driver, svc: Service, target: Option<String>) {
    let label = target.clone().unwrap_or_else(|| pin_default(svc).into());
    send(
        app,
        d,
        Operation::Service(format!("{}: {label} chosen.", service_title(svc))),
        "Checking and choosing provider…",
        |id| Command::ServiceSelect {
            id,
            service: service_name(svc).into(),
            target,
        },
    );
}

pub(super) fn delete(app: &mut App, d: &mut Driver, svc: Service, target: &str) {
    if chosen(app, svc, target) {
        notice(
            app,
            "Choose another provider or the default before deleting this provider.",
            Tone::Error,
        );
        return;
    }
    let Ok(target) = runeengine::settings::services::Target::parse(target) else {
        return;
    };
    match target.route {
        Some(route) => forget_route(app, d, &target.name, svc, &route),
        None => {
            let mut state = ServiceState::of(app, &target.name);
            *state.uses(svc) = None;
            state.send(
                app,
                d,
                &format!("{} removed from this tool. Its account stays.", target.name),
                "Saving…",
            );
        }
    }
}
