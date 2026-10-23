using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;
using Thalamus.Plugin.Interop;

namespace Thalamus.Plugin;

/// <summary>
/// Thalamus's main thread (its io_context), where node callbacks run and
/// where state and nodes may be touched. It's installed as the
/// SynchronizationContext of every callback into this plugin, so an
/// <c>await</c> started there resumes there, like the sidecar's AsyncContext.
/// </summary>
public sealed class MainThread : SynchronizationContext
{
    internal static readonly MainThread Instance = new();

    private static int threadId = -1;
    // Set by thalamus_teardown: the io_context is going away, so stop posting to it.
    private static volatile bool stopped;

    private MainThread() { }

    /// <summary>Whether the caller is on Thalamus's main thread.</summary>
    public static bool IsCurrent => Environment.CurrentManagedThreadId == threadId;

    internal static void Init()
    {
        threadId = Environment.CurrentManagedThreadId;
        Enter();
    }

    internal static void Stop() => stopped = true;

    /// <summary>Called at the start of every callback from Thalamus.</summary>
    internal static void Enter()
    {
        if (Current != Instance)
        {
            SetSynchronizationContext(Instance);
        }
    }

    internal static void Check()
    {
        if (!IsCurrent)
        {
            throw new InvalidOperationException("Must be called on Thalamus's main thread");
        }
    }

    /// <summary>Runs action on the main thread later. Callable from any thread.</summary>
    public static unsafe void Post(Action action)
    {
        if (stopped)
        {
            return;
        }
        var handle = GCHandle.Alloc(action);
        ((delegate* unmanaged<delegate* unmanaged<nint, void>, nint, void>)Api.Slot(Api.IoContextPost))(&RunPosted, GCHandle.ToIntPtr(handle));
    }

    /// <summary>
    /// Runs action on the main thread and completes once it (and the task it
    /// returns) has. Callable from any thread; exceptions go to the result.
    /// </summary>
    public static Task Run(Func<Task> action)
    {
        var result = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        Post(async () =>
        {
            try
            {
                await action();
                result.SetResult();
            }
            catch (Exception ex)
            {
                result.SetException(ex);
            }
        });
        return result.Task;
    }

    public static Task Run(Action action) => Run(() =>
    {
        action();
        return Task.CompletedTask;
    });

    public override void Post(SendOrPostCallback d, object? state) => Post(() => d(state));

    public override void Send(SendOrPostCallback d, object? state)
    {
        if (IsCurrent)
        {
            d(state);
        }
        else
        {
            Run(() => d(state)).Wait();
        }
    }

    public override SynchronizationContext CreateCopy() => this;

    [UnmanagedCallersOnly]
    private static void RunPosted(nint data)
    {
        var handle = GCHandle.FromIntPtr(data);
        var action = (Action)handle.Target!;
        handle.Free();
        Enter();
        try
        {
            action();
        }
        catch (Exception ex)
        {
            ThalamusHost.Report(ex, "main thread callback");
        }
    }
}

public enum DialogType
{
    Info,
    Warn,
    Error,
    Fatal,
}

/// <summary>Host services that aren't tied to a node.</summary>
public static unsafe class ThalamusHost
{
    /// <summary>Thalamus's steady clock, the time base of node data.</summary>
    public static long TimeNs => (long)((delegate* unmanaged<ulong>)Api.Slot(Api.TimeNs))();

    /// <summary>
    /// Sends text to Thalamus's log (log view, recordings). On a Thalamus
    /// without the log slot it's written to stdout instead. Any thread.
    /// </summary>
    public static void Log(string text)
    {
        if (!Api.Has(Api.Log))
        {
            Console.WriteLine(text);
            return;
        }
        var bytes = System.Text.Encoding.UTF8.GetBytes(text);
        fixed (byte* data = bytes)
        {
            var span = new CharSpan { Data = (nint)data, Size = (ulong)bytes.Length };
            ((delegate* unmanaged<CharSpan*, void>)Api.Slot(Api.Log))(&span);
        }
    }

    /// <summary>Shows a dialog in Thalamus's UI. Any thread.</summary>
    public static void ShowDialog(string title, string message, DialogType type)
    {
        if (!MainThread.IsCurrent)
        {
            MainThread.Post(() => ShowDialog(title, message, type));
            return;
        }
        if (!Api.Has(Api.DialogShow))
        {
            Log($"{type}: {title}: {message}");
            return;
        }
        var titleBytes = System.Text.Encoding.UTF8.GetBytes(title);
        var messageBytes = System.Text.Encoding.UTF8.GetBytes(message);
        fixed (byte* titleData = titleBytes)
        fixed (byte* messageData = messageBytes)
        {
            var titleSpan = new CharSpan { Data = (nint)titleData, Size = (ulong)titleBytes.Length };
            var messageSpan = new CharSpan { Data = (nint)messageData, Size = (ulong)messageBytes.Length };
            ((delegate* unmanaged<CharSpan*, CharSpan*, int, void>)Api.Slot(Api.DialogShow))(&titleSpan, &messageSpan, (int)type);
        }
    }

    /// <summary>
    /// Logs an exception that escaped node code. Exceptions can't cross into
    /// Thalamus (that aborts it), so every callback catches and reports them.
    /// </summary>
    internal static void Report(Exception ex, string where, [CallerMemberName] string caller = "")
    {
        var text = $"thalamus_contrib_cs: unhandled exception in {where} ({caller}): {ex}";
        Console.Error.WriteLine(text);
        if (!Api.Has(Api.Log))
        {
            // Log would only print it again.
            return;
        }
        try
        {
            Log(text);
        }
        catch
        {
            // Already on stderr.
        }
    }
}
