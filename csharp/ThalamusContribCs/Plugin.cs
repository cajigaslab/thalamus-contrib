// The node types this library exports, the C# counterpart of export_nodes! in
// rust/src/lib.rs: a node must be listed here to be loaded by Thalamus.
using System.Runtime.InteropServices;
using Thalamus.Plugin;

namespace Thalamus.Contrib;

public static class Plugin
{
    private static readonly NodeType[] Nodes =
    [
        NodeType.Create("DELSYS", context => new DelsysNode(context), DelsysNode.Prepare, DelsysNode.Cleanup),
    ];

    [UnmanagedCallersOnly(EntryPoint = "thalamus_get_node_factories")]
    private static nint GetNodeFactories(nint api) => Host.GetNodeFactories(api, Nodes);
}
