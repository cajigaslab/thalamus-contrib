using System.Runtime.InteropServices;
using System.Text;
using System.Text.Json;
using Thalamus.Plugin.Interop;

namespace Thalamus.Plugin;

public enum StateAction
{
    Set,
    Delete,
}

/// <summary>
/// Called for changes to a state and, recursively, everything in it. source
/// is the collection that changed, key a string (dicts) or long (lists) and
/// value as returned by the <see cref="State"/> indexers.
/// </summary>
public delegate void StateCallback(State source, StateAction action, object key, object? value);

/// <summary>
/// A dict or list in Thalamus's state tree (the config). Values are
/// <c>null</c>, <c>bool</c>, <c>long</c>, <c>double</c>, <c>string</c> or
/// another <see cref="State"/>. Setting a value notifies every connected
/// callback synchronously, including the caller's own: state callbacks are
/// re-entrant, so post changes made from a callback when that matters.
///
/// Only use states on the main thread. Two State objects are equal when they
/// wrap the same collection.
/// </summary>
public sealed unsafe class State : IEquatable<State>
{
    private readonly nint raw;

    /// <summary>Wraps raw, taking a new reference to it.</summary>
    internal State(nint raw)
    {
        this.raw = raw;
        ((delegate* unmanaged<nint, void>)Api.Slot(Api.StateIncRef))(raw);
    }

    ~State()
    {
        // Finalizers run on their own thread; references are released on the main thread.
        var raw = this.raw;
        try
        {
            MainThread.Post(() => ((delegate* unmanaged<nint, void>)Api.Slot(Api.StateDecRef))(raw));
        }
        catch
        {
            // An exception escaping a finalizer kills the process; leaking the reference is better.
        }
    }

    internal nint Raw => raw;

    public bool IsDict => Is(Api.StateIsDict, raw);
    public bool IsList => Is(Api.StateIsList, raw);

    public object? this[string key]
    {
        get
        {
            MainThread.Check();
            var bytes = Encoding.UTF8.GetBytes(key);
            fixed (byte* data = bytes)
            {
                var span = new CharSpan { Data = (nint)data, Size = (ulong)bytes.Length };
                return TakeValue(((delegate* unmanaged<nint, CharSpan*, nint>)Api.Slot(Api.StateGetAtName))(raw, &span));
            }
        }
        set
        {
            MainThread.Check();
            var bytes = Encoding.UTF8.GetBytes(key);
            fixed (byte* data = bytes)
            {
                var span = new CharSpan { Data = (nint)data, Size = (ulong)bytes.Length };
                switch (value)
                {
                    case null:
                        ((delegate* unmanaged<nint, CharSpan*, void>)Api.Slot(Api.StateSetAtNameNull))(raw, &span);
                        break;
                    case bool b:
                        ((delegate* unmanaged<nint, CharSpan*, byte, void>)Api.Slot(Api.StateSetAtNameBool))(raw, &span, b ? (byte)1 : (byte)0);
                        break;
                    case string s:
                        var valueBytes = Encoding.UTF8.GetBytes(s);
                        fixed (byte* valueData = valueBytes)
                        {
                            var valueSpan = new CharSpan { Data = (nint)valueData, Size = (ulong)valueBytes.Length };
                            ((delegate* unmanaged<nint, CharSpan*, CharSpan*, void>)Api.Slot(Api.StateSetAtNameString))(raw, &span, &valueSpan);
                        }
                        break;
                    case State st:
                        ((delegate* unmanaged<nint, CharSpan*, nint, void>)Api.Slot(Api.StateSetAtNameState))(raw, &span, st.raw);
                        break;
                    case double or float:
                        ((delegate* unmanaged<nint, CharSpan*, double, void>)Api.Slot(Api.StateSetAtNameFloat))(raw, &span, Convert.ToDouble(value));
                        break;
                    case long or int or short or sbyte or byte or ushort or uint:
                        ((delegate* unmanaged<nint, CharSpan*, long, void>)Api.Slot(Api.StateSetAtNameInt))(raw, &span, Convert.ToInt64(value));
                        break;
                    default:
                        throw new ArgumentException($"Unsupported state value type {value.GetType()}");
                }
            }
        }
    }

    public object? this[long index]
    {
        get
        {
            MainThread.Check();
            return TakeValue(((delegate* unmanaged<nint, ulong, nint>)Api.Slot(Api.StateGetAtIndex))(raw, (ulong)index));
        }
        set
        {
            MainThread.Check();
            switch (value)
            {
                case null:
                    ((delegate* unmanaged<nint, long, void>)Api.Slot(Api.StateSetAtIndexNull))(raw, index);
                    break;
                case bool b:
                    ((delegate* unmanaged<nint, long, byte, void>)Api.Slot(Api.StateSetAtIndexBool))(raw, index, b ? (byte)1 : (byte)0);
                    break;
                case string s:
                    var valueBytes = Encoding.UTF8.GetBytes(s);
                    fixed (byte* valueData = valueBytes)
                    {
                        var valueSpan = new CharSpan { Data = (nint)valueData, Size = (ulong)valueBytes.Length };
                        ((delegate* unmanaged<nint, long, CharSpan*, void>)Api.Slot(Api.StateSetAtIndexString))(raw, index, &valueSpan);
                    }
                    break;
                case State st:
                    ((delegate* unmanaged<nint, long, nint, void>)Api.Slot(Api.StateSetAtIndexState))(raw, index, st.raw);
                    break;
                case double or float:
                    ((delegate* unmanaged<nint, long, double, void>)Api.Slot(Api.StateSetAtIndexFloat))(raw, index, Convert.ToDouble(value));
                    break;
                case long or int or short or sbyte or byte or ushort or uint:
                    ((delegate* unmanaged<nint, long, long, void>)Api.Slot(Api.StateSetAtIndexInt))(raw, index, Convert.ToInt64(value));
                    break;
                default:
                    throw new ArgumentException($"Unsupported state value type {value.GetType()}");
            }
        }
    }

    /// <summary>The keys (string or long) and values of this dict or list.</summary>
    public List<KeyValuePair<object, object?>> Items()
    {
        MainThread.Check();
        var result = new List<KeyValuePair<object, object?>>();
        var iter = ((delegate* unmanaged<nint, nint>)Api.Slot(Api.StateIterCreate))(raw);
        try
        {
            while (((delegate* unmanaged<nint, byte>)Api.Slot(Api.StateIterNext))(iter) != 0)
            {
                var key = TakeValue(((delegate* unmanaged<nint, nint>)Api.Slot(Api.StateIterKey))(iter))!;
                var value = TakeValue(((delegate* unmanaged<nint, nint>)Api.Slot(Api.StateIterValue))(iter));
                result.Add(new KeyValuePair<object, object?>(key, value));
            }
        }
        finally
        {
            ((delegate* unmanaged<nint, void>)Api.Slot(Api.StateIterDestroy))(iter);
        }
        return result;
    }

    public IEnumerable<object?> Values() => Items().Select(i => i.Value);

    /// <summary>
    /// Calls callback for every change to this state and the collections in
    /// it until the returned connection is disposed (on the main thread).
    /// </summary>
    public IDisposable Connect(StateCallback callback)
    {
        MainThread.Check();
        var handle = GCHandle.Alloc(callback);
        var connection = ((delegate* unmanaged<nint, delegate* unmanaged<nint, int, nint, nint, nint, void>, nint, nint>)Api.Slot(Api.StateRecursiveChangeConnect))(
            raw, &OnChange, GCHandle.ToIntPtr(handle));
        return new Connection(connection, handle);
    }

    /// <summary>Calls callback with a Set for every entry, as if each were just set.</summary>
    public void Recap(StateCallback callback)
    {
        MainThread.Check();
        var handle = GCHandle.Alloc(callback);
        try
        {
            ((delegate* unmanaged<nint, delegate* unmanaged<nint, int, nint, nint, nint, void>, nint, void>)Api.Slot(Api.StateRecapWith))(
                raw, &OnChange, GCHandle.ToIntPtr(handle));
        }
        finally
        {
            handle.Free();
        }
    }

    /// <summary>This state as JSON, e.g. to compare snapshots of it.</summary>
    public string ToJson()
    {
        using var stream = new MemoryStream();
        using (var writer = new Utf8JsonWriter(stream))
        {
            Write(writer, this);
        }
        return Encoding.UTF8.GetString(stream.ToArray());
    }

    public bool Equals(State? other) => other is not null && other.raw == raw;
    public override bool Equals(object? obj) => obj is State other && Equals(other);
    public override int GetHashCode() => raw.GetHashCode();
    public static bool operator ==(State? a, State? b) => a is null ? b is null : a.Equals(b);
    public static bool operator !=(State? a, State? b) => !(a == b);

    private static void Write(Utf8JsonWriter writer, object? value)
    {
        switch (value)
        {
            case null:
                writer.WriteNullValue();
                break;
            case bool b:
                writer.WriteBooleanValue(b);
                break;
            case long l:
                writer.WriteNumberValue(l);
                break;
            case double d:
                writer.WriteNumberValue(d);
                break;
            case string s:
                writer.WriteStringValue(s);
                break;
            case State st when st.IsDict:
                writer.WriteStartObject();
                foreach (var item in st.Items())
                {
                    writer.WritePropertyName((string)item.Key);
                    Write(writer, item.Value);
                }
                writer.WriteEndObject();
                break;
            case State st:
                writer.WriteStartArray();
                foreach (var item in st.Values())
                {
                    Write(writer, item);
                }
                writer.WriteEndArray();
                break;
        }
    }

    private static bool Is(int slot, nint raw) => ((delegate* unmanaged<nint, byte>)Api.Slot(slot))(raw) != 0;

    /// <summary>Converts a borrowed state value, wrapping collections in new references.</summary>
    internal static object? Wrap(nint raw)
    {
        if (raw == 0 || Is(Api.StateIsNull, raw))
        {
            return null;
        }
        // Bool before int, as Rust's wrap_state does.
        if (Is(Api.StateIsBool, raw))
        {
            return ((delegate* unmanaged<nint, byte>)Api.Slot(Api.StateGetBool))(raw) != 0;
        }
        if (Is(Api.StateIsDict, raw) || Is(Api.StateIsList, raw))
        {
            return new State(raw);
        }
        if (Is(Api.StateIsFloat, raw))
        {
            return ((delegate* unmanaged<nint, double>)Api.Slot(Api.StateGetFloat))(raw);
        }
        if (Is(Api.StateIsInt, raw))
        {
            return ((delegate* unmanaged<nint, long>)Api.Slot(Api.StateGetInt))(raw);
        }
        if (Is(Api.StateIsString, raw))
        {
            // Borrowed (owns_data is 0), valid while raw is.
            CharSpan span;
            ((delegate* unmanaged<CharSpan*, nint, void>)Api.Slot(Api.StateGetString))(&span, raw);
            return Api.ToManaged(span);
        }
        return null;
    }

    /// <summary>Converts a state value the caller owns a reference to, releasing it.</summary>
    private static object? TakeValue(nint raw)
    {
        if (raw == 0)
        {
            return null;
        }
        try
        {
            return Wrap(raw);
        }
        finally
        {
            ((delegate* unmanaged<nint, void>)Api.Slot(Api.StateDecRef))(raw);
        }
    }

    [UnmanagedCallersOnly]
    private static void OnChange(nint source, int action, nint key, nint value, nint data)
    {
        MainThread.Enter();
        try
        {
            var callback = (StateCallback)GCHandle.FromIntPtr(data).Target!;
            callback(new State(source), action == 0 ? StateAction.Set : StateAction.Delete, Wrap(key)!, Wrap(value));
        }
        catch (Exception ex)
        {
            ThalamusHost.Report(ex, "state callback");
        }
    }

    private sealed class Connection(nint connection, GCHandle handle) : IDisposable
    {
        private bool disposed;

        public void Dispose()
        {
            if (disposed)
            {
                return;
            }
            MainThread.Check();
            disposed = true;
            ((delegate* unmanaged<nint, void>)Api.Slot(Api.StateRecursiveChangeDisconnect))(connection);
            handle.Free();
        }
    }
}
