using System.Text.Json.Nodes;

namespace Thalamus.Plugin;

/// <summary>
/// Base class of C# nodes. Nodes are created, called and disposed on
/// Thalamus's main thread; work on other threads comes back with
/// <see cref="MainThread.Post"/> or <see cref="MainThread.Run(Func{Task})"/>.
/// Implement <see cref="IAnalogNode"/> and/or <see cref="ITextNode"/> for the
/// data the node produces and call <see cref="Ready()"/> after each message.
/// </summary>
public abstract class Node : IDisposable
{
    private readonly NodeContext context;

    protected Node(NodeContext context)
    {
        this.context = context;
    }

    /// <summary>The node's state: its entry in the config's node list.</summary>
    protected State State => context.State;

    /// <summary>
    /// Tells Thalamus a new message is ready, timed timeNs (Thalamus's steady
    /// clock, <see cref="ThalamusHost.TimeNs"/>). Subscribers read the data
    /// synchronously, so it must stay unchanged until Ready returns. Main
    /// thread only; does nothing during the constructor or after Dispose.
    /// </summary>
    protected void Ready(long timeNs) => context.Ready(timeNs);

    /// <summary>Ready, timed now.</summary>
    protected void Ready() => context.Ready(ThalamusHost.TimeNs);

    /// <summary>
    /// Handles a node_request (e.g. from the node's widget). The response is
    /// sent when the task completes; an exception responds with
    /// <c>{"error": message}</c>.
    /// </summary>
    public virtual Task<JsonNode?> Process(JsonNode? request) => Task.FromResult<JsonNode?>(null);

    /// <summary>
    /// Runs before the node is destroyed (removed from the config or Thalamus
    /// shutting down); Thalamus waits for the task before calling Dispose.
    /// </summary>
    public virtual Task Predrop() => Task.CompletedTask;

    /// <summary>Releases the node's resources (state connections, devices).</summary>
    public virtual void Dispose() { }
}

public interface IAnalogNode
{
    int NumChannels();
    long SampleIntervalNs(int channel);
    string Name(int channel);
    bool HasAnalogData();
    /// <summary>The channel's samples in the current message.</summary>
    ReadOnlySpan<double> Data(int channel);
    /// <summary>
    /// Whether the channels (count, names or sample intervals) differ from the
    /// previous analog message's. Only the first message after a change reports it.
    /// </summary>
    bool ChannelsChanged();
}

public interface ITextNode
{
    string Text();
    bool HasTextData();
}

/// <summary>Links a node to its Thalamus counterpart; passed to the node's constructor.</summary>
public sealed unsafe class NodeContext
{
    private readonly Interop.ThalamusNode* native;

    internal NodeContext(Interop.ThalamusNode* native, State state)
    {
        this.native = native;
        State = state;
    }

    internal State State { get; }
    internal bool Constructed { get; set; }
    internal bool Destroyed { get; set; }
    internal long TimeNs { get; private set; }

    internal void Ready(long timeNs)
    {
        MainThread.Check();
        if (!Constructed || Destroyed)
        {
            return;
        }
        TimeNs = timeNs;
        ((delegate* unmanaged<Interop.ThalamusNode*, void>)Interop.Api.Slot(Interop.Api.NodeReady))(native);
    }
}

/// <summary>
/// A node type exported by the plugin, listed in thalamus_get_node_factories.
/// prepare runs once when Thalamus loads the plugin (false hides the type),
/// cleanup when it unloads.
/// </summary>
public sealed class NodeType
{
    internal NodeType(string name, Func<NodeContext, Node> create, Func<bool>? prepare, Action? cleanup)
    {
        Name = name;
        CreateNode = create;
        PrepareType = prepare ?? (() => true);
        CleanupType = cleanup ?? (() => { });
    }

    public string Name { get; }
    internal Func<NodeContext, Node> CreateNode { get; }
    internal Func<bool> PrepareType { get; }
    internal Action CleanupType { get; }

    public static NodeType Create<T>(string name, Func<NodeContext, T> create, Func<bool>? prepare = null, Action? cleanup = null)
        where T : Node
    {
        return new NodeType(name, context => create(context), prepare, cleanup);
    }
}
