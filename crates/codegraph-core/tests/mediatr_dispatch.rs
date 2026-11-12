//! MediatR Send/Publish → the `Handle` of the matching handler, read through any interface whose
//! base list and `Handle` both name the type (upstream 84c7fbae; port of
//! mediatr-dispatch-synthesizer.test.ts).
use codegraph_core::CodeGraph;
use codegraph_types::NodeKind;
use std::fs;

/// One `mediatr-dispatch` edge: (source name, source file, target name, target file, target line, via).
type Hop = (String, String, String, String, u32, String);

fn hops(files: &[(&str, &str)]) -> (Vec<Hop>, Vec<codegraph_types::Node>) {
    let dir = tempfile::tempdir().unwrap();
    for (rel, text) in files {
        let path = dir.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    let nodes = cg.get_all_nodes().unwrap();
    let by_id: std::collections::HashMap<&str, &codegraph_types::Node> = nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    let mut out = Vec::new();
    for n in &nodes {
        for e in cg.get_outgoing_edges(&n.id).unwrap() {
            let meta = e.metadata.as_ref();
            if meta.and_then(|m| m.get("synthesizedBy")).and_then(|v| v.as_str()) != Some("mediatr-dispatch") {
                continue;
            }
            let t = by_id[e.target.as_str()];
            let via = meta.and_then(|m| m.get("via")).and_then(|v| v.as_str()).unwrap_or("").to_string();
            out.push((n.name.clone(), n.file_path.clone(), t.name.clone(), t.file_path.clone(), t.start_line, via));
        }
    }
    cg.close();
    (out, nodes)
}

/// Every bridged hop as `Source > HandlerClass via Type`, sorted.
fn bridges(files: &[(&str, &str)]) -> Vec<String> {
    let (hops, nodes) = hops(files);
    let mut out: Vec<String> = hops
        .iter()
        .map(|(src, _, _, file, line, via)| {
            let owner = nodes
                .iter()
                .filter(|n| n.kind == NodeKind::Class && &n.file_path == file && n.start_line <= *line && n.end_line >= *line)
                .max_by_key(|n| n.start_line)
                .map_or("?", |n| n.name.as_str());
            format!("{src} > {owner} via {via}")
        })
        .collect();
    out.sort();
    out
}

const SENDER: &str = r#"namespace Shop;
using System.Threading.Tasks;
public class ThingsController {
    private readonly ISender _mediator;
    public ThingsController(ISender mediator) { _mediator = mediator; }
    public async Task GetThings() { await _mediator.Send(new GetThingsQuery()); }
    public async Task Create(CreateThingCommand command) { await _mediator.Send(command); }
}
"#;

#[test]
fn bridges_send_and_publish_across_inline_local_and_param_arguments() {
    let (rows, _) = hops(&[
        ("Requests.cs", r#"namespace Shop;
using MediatR;
public record GetThingsQuery : IRequest<ThingsVm>;
public record CreateThingCommand(string Name) : IRequest<int>;
public record DeleteThingCommand(int Id) : IRequest;
public record ThingDeletedNotification(int Id) : INotification;
public class UnhandledCommand : IRequest<int> { }
"#),
        ("Handlers.cs", r#"namespace Shop;
using MediatR;
using System.Threading;
using System.Threading.Tasks;
public class GetThingsQueryHandler : IRequestHandler<GetThingsQuery, ThingsVm> {
    public Task<ThingsVm> Handle(GetThingsQuery request, CancellationToken ct) => Task.FromResult(new ThingsVm());
}
public class CreateThingCommandHandler : IRequestHandler<CreateThingCommand, int> {
    public Task<int> Handle(CreateThingCommand request, CancellationToken ct) => Task.FromResult(1);
}
public class DeleteThingCommandHandler : IRequestHandler<DeleteThingCommand> {
    public Task Handle(DeleteThingCommand request, CancellationToken ct) => Task.CompletedTask;
}
public class ThingDeletedNotificationHandler : INotificationHandler<ThingDeletedNotification> {
    public Task Handle(ThingDeletedNotification notification, CancellationToken ct) => Task.CompletedTask;
}
"#),
        ("ThingsController.cs", r#"namespace Shop;
using MediatR;
using System.Threading.Tasks;
public class ThingsController {
    private readonly ISender _mediator;
    public ThingsController(ISender mediator) { _mediator = mediator; }

    public async Task GetThings() {
        var vm = await _mediator.Send(new GetThingsQuery());
    }
    public async Task Create(CreateThingCommand command) {
        var id = await _mediator.Send(command);
    }
    public async Task Delete(int id) {
        var command = new DeleteThingCommand(id);
        await _mediator.Send(command);
    }
    public async Task Notify(int id) {
        await _mediator.Publish(new ThingDeletedNotification(id));
    }
    public async Task Bogus() {
        await _mediator.Send(new UnhandledCommand());
    }
    public void ViaMessagingCenter() {
        MessagingCenter.Send(this, "evt", new CreateThingCommand("x"));
    }
}
"#),
    ]);
    let mut sources: Vec<&str> = rows.iter().map(|r| r.0.as_str()).collect();
    sources.sort();
    assert_eq!(sources, ["Create", "Delete", "GetThings", "Notify"]);
    let mut via: Vec<&str> = rows.iter().map(|r| r.5.as_str()).collect();
    via.sort();
    via.dedup();
    assert_eq!(via, ["CreateThingCommand", "DeleteThingCommand", "GetThingsQuery", "ThingDeletedNotification"]);
    assert!(rows.iter().all(|r| r.2 == "Handle"));
    assert!(!rows.iter().any(|r| r.5 == "UnhandledCommand"));
    assert!(!rows.iter().any(|r| r.0 == "ViaMessagingCenter"));
}

#[test]
fn reads_a_handler_through_an_interface_the_project_derives_from_mediatr() {
    let got = bridges(&[
        ("Abstractions.cs", r#"namespace Shop;
using MediatR;
public interface ICommandHandler<in TCommand, TResult> : IRequestHandler<TCommand, TResult> where TCommand : ICommand<TResult> { }
public interface IQueryHandler<in TQuery, TResult> : IRequestHandler<TQuery, TResult> where TQuery : IQuery<TResult> { }
"#),
        ("Handlers.cs", r#"namespace Shop;
public class GetThingsQueryHandler : IQueryHandler<GetThingsQuery, ThingsVm> {
    public Task<ThingsVm> Handle(GetThingsQuery request, CancellationToken ct) => Task.FromResult(new ThingsVm());
}
public class CreateThingCommandHandler : ICommandHandler<CreateThingCommand, int> {
    public Task<int> Handle(CreateThingCommand request, CancellationToken ct) => Task.FromResult(1);
}
"#),
        ("ThingsController.cs", SENDER),
    ]);
    assert_eq!(got, ["Create > CreateThingCommandHandler via CreateThingCommand", "GetThings > GetThingsQueryHandler via GetThingsQuery"]);
}

#[test]
fn reads_a_handler_whose_interface_comes_from_an_unindexed_package() {
    let got = bridges(&[
        ("Handlers.cs", r#"namespace Shop;
using Mediator;
public class GetThingsHandler(IReadRepository<Thing> repository, ILogger<GetThingsHandler> logger)
  : IQueryHandler<GetThingsQuery, Result<ThingsVm>>
{
  public async ValueTask<Result<ThingsVm>> Handle(GetThingsQuery request, CancellationToken ct) => default;
}
public sealed class CreateThingHandler : Shared.Kernel.ICommandHandler<Commands.CreateThingCommand>
{
  public ValueTask Handle(Commands.CreateThingCommand command, CancellationToken ct) => default;
}
"#),
        ("ThingsController.cs", SENDER),
    ]);
    assert_eq!(got, ["Create > CreateThingHandler via CreateThingCommand", "GetThings > GetThingsHandler via GetThingsQuery"]);
}

#[test]
fn reads_each_handle_of_a_class_that_handles_several_types() {
    let got = bridges(&[
        ("Handlers.cs", r#"namespace Shop;
public class AuditHandler : IDomainEventHandler<ThingCreated>, IDomainEventHandler<ThingDeleted> {
    public Task Handle(ThingCreated notification, CancellationToken ct) => Task.CompletedTask;
    public Task Handle(ThingDeleted notification, CancellationToken ct) => Task.CompletedTask;
}
"#),
        ("Service.cs", r#"namespace Shop;
public class Service {
    private readonly IPublisher _publisher;
    public async Task Created() { await _publisher.Publish(new ThingCreated()); }
    public async Task Deleted() { await _publisher.Publish(new ThingDeleted()); }
}
"#),
    ]);
    assert_eq!(got, ["Created > AuditHandler via ThingCreated", "Deleted > AuditHandler via ThingDeleted"]);
}

#[test]
fn takes_the_type_a_mapper_builds_as_what_is_sent() {
    let got = bridges(&[
        ("Handlers.cs", r#"namespace Shop;
public class CreateThingHandler : ICommandHandler<CreateThing, int> {
    public Task<int> Handle(CreateThing command, CancellationToken ct) => Task.FromResult(1);
}
public class UpdateThingHandler : ICommandHandler<UpdateThing, int> {
    public Task<int> Handle(UpdateThing command, CancellationToken ct) => Task.FromResult(1);
}
"#),
        ("Endpoint.cs", r#"namespace Shop;
public class Endpoint {
    public async Task Create(CreateThingDto dto, IMediator mediator, IMapper mapper) {
        var command = mapper.Map<CreateThing>(dto);
        await mediator.Send(command);
    }
    public async Task Update(UpdateThingDto dto, IMediator mediator) {
        var command = dto.Adapt<UpdateThingDto, Commands.UpdateThing>();
        await mediator.Send(command);
    }
}
"#),
    ]);
    assert_eq!(got, ["Create > CreateThingHandler via CreateThing", "Update > UpdateThingHandler via UpdateThing"]);
}

#[test]
fn keeps_two_solutions_apart_when_a_type_has_the_same_name_in_both() {
    let mut owned: Vec<(String, String)> = Vec::new();
    for root in ["src", "sample"] {
        owned.push((format!("{root}/UseCases/CreateHandler.cs"), format!(r#"namespace {root};
public class CreateThingHandler : ICommandHandler<CreateThing, int> {{
    public Task<int> Handle(CreateThing command, CancellationToken ct) => Task.FromResult(1);
}}
public class ThingCreatedHandler : INotificationHandler<ThingCreated> {{
    public Task Handle(ThingCreated notification, CancellationToken ct) => Task.CompletedTask;
}}
"#)));
        owned.push((format!("{root}/Web/Create.cs"), format!(r#"namespace {root};
public class Create{root} {{
    private readonly IMediator _mediator;
    public async Task Run() {{ await _mediator.Send(new CreateThing()); }}
    public async Task Tell() {{ await _mediator.Publish(new ThingCreated()); }}
}}
"#)));
    }
    let files: Vec<(&str, &str)> = owned.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    let (rows, _) = hops(&files);
    assert_eq!(rows.len(), 4, "{rows:?}");
    for r in &rows {
        assert_eq!(r.1.split('/').next(), r.3.split('/').next(), "{r:?}");
    }
}

#[test]
fn lets_a_notification_reach_every_module_of_a_solution() {
    let got = bridges(&[
        ("src/Orders/Service.cs", r#"namespace Orders;
public class Service {
    private readonly IPublisher _publisher;
    public async Task Tell() { await _publisher.Publish(new ThingCreated()); }
}
"#),
        ("src/Orders/Handlers/Audit.cs", r#"namespace Orders;
public class AuditHandler : INotificationHandler<ThingCreated> {
    public Task Handle(ThingCreated notification, CancellationToken ct) => Task.CompletedTask;
}
"#),
        ("src/Billing/Handlers/Charge.cs", r#"namespace Billing;
public class ChargeHandler : INotificationHandler<ThingCreated> {
    public Task Handle(ThingCreated notification, CancellationToken ct) => Task.CompletedTask;
}
"#),
    ]);
    assert_eq!(got, ["Tell > AuditHandler via ThingCreated", "Tell > ChargeHandler via ThingCreated"]);
}

#[test]
fn wants_the_type_in_both_the_base_list_and_handle() {
    let got = bridges(&[
        ("Handlers.cs", r#"namespace Shop;
public class NoBaseList {
    public Task Handle(GetThingsQuery request, CancellationToken ct) => Task.CompletedTask;
}
public class OtherType : IThingHandler<GetThingsQuery> {
    public Task Handle(string request, CancellationToken ct) => Task.CompletedTask;
}
public class Decorator<T> : ICommandHandler<T> {
    public Task Handle(T command, CancellationToken ct) => Task.CompletedTask;
}
public class ConstructorOnly(IRepository<CreateThingCommand> repository) : IThingService {
    public Task Handle(CreateThingCommand command, CancellationToken ct) => Task.CompletedTask;
}
"#),
        ("ThingsController.cs", SENDER),
        ("Generic.cs", r#"namespace Shop;
public class Generic {
    private readonly IMediator _mediator;
    public async Task Run<T>(T command) { await _mediator.Send(command); }
}
"#),
    ]);
    assert!(got.is_empty(), "{got:?}");
}

#[test]
fn produces_no_edges_in_a_csharp_project_without_mediatr() {
    let (rows, _) = hops(&[("Service.cs", r#"namespace Shop;
public class Service {
    private readonly IRepo _repo;
    public Service(IRepo repo) { _repo = repo; }
    public string Find(string id) => _repo.Get(id);
}
"#)]);
    assert!(rows.is_empty(), "{rows:?}");
}
