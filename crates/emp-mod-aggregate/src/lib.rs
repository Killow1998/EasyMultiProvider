use emp_modules::{
    Command,
    aggregate::{BindingPlan, Policy, Request, Source},
};
use serde_json::{Value, json};
use std::{cmp::Ordering, collections::BTreeSet};

pub fn handle(command: Command) -> Result<Value, &'static str> {
    match command {
        Command::Aggregate { request } => {
            serde_json::to_value(binding_plan(request)?).map_err(|_| "invalid_module_reply")
        }
        Command::AggregateOrder { policy, candidates } => {
            let policy = Policy::parse(&policy).unwrap_or(Policy::Priority);
            let order = rank(
                policy,
                candidates
                    .iter()
                    .enumerate()
                    .map(|(id, candidate)| (id, candidate.capacity, candidate.plan.as_deref())),
            )?;
            Ok(json!({"order": order, "rotate": policy == Policy::RoundRobin}))
        }
        _ => Err("invalid_module_command"),
    }
}

fn binding_plan(request: Request) -> Result<BindingPlan, &'static str> {
    let mut owners = BTreeSet::new();
    let eligible = request.candidates.iter().filter(|candidate| {
        let excluded = candidate.cooling_down
            || candidate.headroom.is_some_and(|headroom| headroom <= 0.0)
            || candidate.source == Source::LocalNativeApiKey
            || (request.policy == Policy::Balanced && candidate.source == Source::NativeApiKey);
        !excluded && owners.insert(candidate.owner_id)
    });
    let candidate_ids = rank(
        request.policy,
        eligible.map(|candidate| {
            (
                candidate.id,
                candidate.headroom,
                candidate.subscription_plan.as_deref(),
            )
        }),
    )?;
    Ok(BindingPlan {
        candidate_ids,
        rotate: request.policy == Policy::RoundRobin,
    })
}

fn rank<'a>(
    policy: Policy,
    candidates: impl Iterator<Item = (usize, Option<f64>, Option<&'a str>)>,
) -> Result<Vec<usize>, &'static str> {
    if policy != Policy::Balanced {
        return Ok(candidates.map(|(id, _, _)| id).collect());
    }
    let mut order = Vec::new();
    let mut plans = BTreeSet::new();
    for (id, headroom, plan) in candidates {
        if let Some(plan) = plan {
            plans.insert(plan);
            if plans.len() > 1 {
                return Err("mixed_subscription_plans");
            }
        }
        order.push((id, headroom));
    }
    order.sort_by(|(_, left), (_, right)| right.partial_cmp(left).unwrap_or(Ordering::Equal));
    Ok(order.into_iter().map(|(id, _)| id).collect())
}
