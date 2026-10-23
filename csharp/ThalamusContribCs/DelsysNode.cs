// Delsys Trigno EMG over DelsysAPI, moved from Thalamus's dotnet sidecar
// (dotnet/dotnet/DelsysNode.cs). The state keys and requests are the ones
// Thalamus's DelsysWidget uses.
using System.Text.Json.Nodes;
using Aero.PipeLine;
using DelsysAPI.DelsysDevices;
using DelsysAPI.Pipelines;
using Thalamus.Plugin;

namespace Thalamus.Contrib;

public sealed class DelsysNode : Node, IAnalogNode, ITextNode
{
    private string? key = null;
    private string? license = null;
    private DeviceSourcePortable? deviceSourceCreator = null;
    private IDelsysDevice? deviceSource = null;
    private Pipeline? pipeline = null;
    private readonly IDisposable stateConnection;

    private CancellationTokenSource? cancellation = null;
    private bool hasTextData = false;
    private bool hasAnalogData = false;
    private bool channelsChanged = true;
    private string text = "";
    private bool scanned = false;
    // Whether the pipeline was started and its CollectionComplete is still to come.
    private bool collecting = false;

    private readonly List<string> channelNames = [];
    private readonly Dictionary<(Guid, Guid), int> channelToIndex = [];
    private readonly List<long> sampleIntervals = [];
    private readonly List<int> sampleCounts = [];
    private readonly List<ArraySegment<double>> segments = [];
    private readonly List<double[]> samples = [];
    private State? components = null;
    private string? lastComponents = null;
    private Task currentTask = Task.CompletedTask;
    private TaskCompletionSource collectionCompleteSource = new();
    private volatile bool busy = false;

    /// <summary>Runs action after every task queued before it has finished.</summary>
    private Task QueueTask(Func<Task> action)
    {
        currentTask = Chain(currentTask, action);
        return currentTask;
    }

    private async Task Chain(Task previous, Func<Task> action)
    {
        try
        {
            await previous;
        }
        catch
        {
            // Already reported by the task that threw.
        }
        try
        {
            await action();
        }
        catch (Exception ex)
        {
            EmitText($"Error: {ex.Message}");
            ThalamusHost.Log($"DELSYS: {ex}");
        }
    }

    public DelsysNode(NodeContext context) : base(context)
    {
        stateConnection = State.Connect(OnChange);
        State.Recap(OnChange);
    }

    public override void Dispose()
    {
        stateConnection.Dispose();
        cancellation?.Cancel();
        if (pipeline != null)
        {
            // The pipeline is a process wide singleton that outlives this
            // node; detach it so a new node can create its own.
            pipeline.CollectionDataReady -= OnData;
            pipeline.TrignoRfManager.ComponentAdded -= Log<DelsysAPI.Events.ComponentAddedEventArgs>;
            pipeline.TrignoRfManager.ComponentLost -= Log<DelsysAPI.Events.ComponentLostEventArgs>;
            pipeline.TrignoRfManager.ComponentRemoved -= Log<DelsysAPI.Events.ComponentRemovedEventArgs>;
            pipeline.TrignoRfManager.ComponentScanComplete -= Log<DelsysAPI.Events.ComponentScanCompletedEventArgs>;
            pipeline.CollectionStarted -= Log<DelsysAPI.Events.CollectionStartedEvent>;
            pipeline.CollectionComplete -= Log<DelsysAPI.Events.CollectionCompleteEvent>;
            pipeline.CollectionComplete -= OnCollectionComplete;
            try
            {
                PipelineController.Instance.RemovePipeline(0);
            }
            catch (Exception ex)
            {
                ThalamusHost.Log($"DELSYS: failed to remove pipeline: {ex.Message}");
            }
            pipeline = null;
        }
    }

    /// <summary>Stops collecting before the node is destroyed, waiting up to 5 s for the device.</summary>
    public override async Task Predrop()
    {
        if (!collecting)
        {
            return;
        }
        var stop = QueueTask(StopPipeline);
        await Task.WhenAny(stop, Task.Delay(TimeSpan.FromSeconds(5)));
    }

    public static bool Prepare()
    {
        // DelsysAPI P/Invokes its USB drivers by file name; they're shipped next to this library.
        NativeLibraries.ResolveFromPluginDirectory(typeof(DeviceSourcePortable).Assembly);
        return true;
    }

    public static void Cleanup()
    {
    }

    private void CreateDataSource()
    {
        if (deviceSourceCreator != null)
        {
            return;
        }

        var abort = false;
        if (string.IsNullOrEmpty(key))
        {
            EmitText("Missing Key");
            abort = true;
        }

        if (string.IsNullOrEmpty(license))
        {
            EmitText("Missing License");
            abort = true;
        }

        if (abort)
        {
            return;
        }

        try
        {
            deviceSourceCreator = new DeviceSourcePortable(key, license);
            deviceSourceCreator.SetDebugOutputStream((str, args) =>
            {
                var text = string.Format(str, args);
                MainThread.Post(() =>
                {
                    EmitText(text);
                    Console.WriteLine(text);
                });
            });
            deviceSource = deviceSourceCreator.GetDataSource(SourceType.TRIGNO_RF);
            deviceSource.Key = key;
            deviceSource.License = license;

            PipelineController.Instance.AddPipeline(deviceSource);
            pipeline = PipelineController.Instance.PipelineIds[0];
            pipeline.TrignoRfManager.ComponentAdded += Log<DelsysAPI.Events.ComponentAddedEventArgs>;
            pipeline.TrignoRfManager.ComponentLost += Log<DelsysAPI.Events.ComponentLostEventArgs>;
            pipeline.TrignoRfManager.ComponentRemoved += Log<DelsysAPI.Events.ComponentRemovedEventArgs>;
            pipeline.TrignoRfManager.ComponentScanComplete += Log<DelsysAPI.Events.ComponentScanCompletedEventArgs>;

            pipeline.CollectionStarted += Log<DelsysAPI.Events.CollectionStartedEvent>;
            pipeline.CollectionDataReady += OnData;
            pipeline.CollectionComplete += Log<DelsysAPI.Events.CollectionCompleteEvent>;
            pipeline.CollectionComplete += OnCollectionComplete;
            EmitText("Connected");
        }
        catch (DelsysAPI.Exceptions.PipelineException ex)
        {
            if (PipelineController.Instance.PipelineIds.Count != 0)
            {
                PipelineController.Instance.RemovePipeline(0);
            }
            pipeline = null;
            deviceSource = null;
            deviceSourceCreator = null;
            EmitText($"Delsys Error: {ex.Message}");
            ThalamusHost.ShowDialog("Delsys Error", ex.Message, DialogType.Error);
        }
    }

    private void OnCollectionComplete(object? sender, DelsysAPI.Events.CollectionCompleteEvent e)
    {
        collectionCompleteSource.TrySetResult();
    }

    private void OnChange(State source, StateAction action, object key, object? value)
    {
        if (source == State)
        {
            if (key is string strKey)
            {
                if (strKey == "Running")
                {
                    channelsChanged = true;
                    CreateDataSource();
                    if (pipeline == null)
                    {
                        return;
                    }
                    var running = value is true;
                    var componentsJson = components?.ToJson();
                    var needConfig = componentsJson != lastComponents;
                    lastComponents = componentsJson;
                    EmitText($"needConfig == {needConfig}");

                    if (running)
                    {
                        // Read on the main thread now, not when the queued task runs.
                        var sampleModes = SelectedSampleModes();
                        QueueTask(() => StartPipeline(needConfig, sampleModes));
                    }
                    else
                    {
                        EmitText("Stop Pipeline");
                        QueueTask(StopPipeline);
                    }
                }
                else if (strKey == "Components")
                {
                    components = value as State;
                    components?.Recap(OnChange);
                }
                else if (strKey == "Key File")
                {
                    if (value is string filename && File.Exists(filename))
                    {
                        this.key = File.ReadAllText(filename);
                    }
                }
                else if (strKey == "License File")
                {
                    if (value is string filename && File.Exists(filename))
                    {
                        license = File.ReadAllText(filename);
                    }
                }
            }
        }
        else if (components != null && source == components)
        {
            if (value is State row)
            {
                row["Ready"] = false;
            }
        }
    }

    /// <summary>The Sample Mode of each selected row of Components, by ID.</summary>
    private Dictionary<long, string> SelectedSampleModes()
    {
        Dictionary<long, string> sampleModes = [];
        if (components != null)
        {
            foreach (var row in components.Values())
            {
                if (row is State rowState && rowState["Selected"] is true && rowState["ID"] is long rowId)
                {
                    sampleModes[rowId] = rowState["Sample Mode"] as string ?? "";
                }
            }
        }
        return sampleModes;
    }

    private async Task StartPipeline(bool needConfig, Dictionary<long, string> sampleModes)
    {
        var pipeline = this.pipeline;
        if (pipeline == null)
        {
            return;
        }
        if (!scanned)
        {
            await Process(new JsonObject { ["type"] = "scan" });
        }
        if (needConfig)
        {
            if (pipeline.CurrentState == Pipeline.ProcessState.Armed)
            {
                await pipeline.DisarmPipeline();
            }
            channelToIndex.Clear();
            channelNames.Clear();
            sampleIntervals.Clear();
            sampleCounts.Clear();
            segments.Clear();
            samples.Clear();

            foreach (var comp in pipeline.TrignoRfManager.Components)
            {
                if (!sampleModes.ContainsKey(comp.PairNumber))
                {
                    EmitText($"Deselect {comp.PairNumber}");
                    var deselectStatus = false;
                    if (comp.State == DelsysAPI.Utils.SelectionState.Allocated)
                    {
                        deselectStatus = await pipeline.TrignoRfManager.DeselectComponentAsync(comp);
                    }
                    EmitText($"Status {deselectStatus}");
                    continue;
                }

                EmitText($"Select {comp.PairNumber}");
                var status = false;
                if (comp.State != DelsysAPI.Utils.SelectionState.Allocated)
                {
                    status = await pipeline.TrignoRfManager.SelectComponentAsync(comp);
                }
                EmitText($"Status {status}");

                var sampleModeIndex = Math.Max(0, Array.IndexOf(comp.Configuration.SampleModes, sampleModes[comp.PairNumber]));
                var sampleMode = comp.Configuration.SampleModes[sampleModeIndex];
                comp.SelectSampleMode(sampleMode);
            }

            EmitText("Configure Pipeline");
            var dataLine = new DataLine(pipeline);
            dataLine.ConfigurePipeline();

            foreach (var comp in pipeline.TrignoRfManager.Components)
            {
                if (!sampleModes.ContainsKey(comp.PairNumber))
                {
                    continue;
                }
                foreach (var channel in comp.TrignoChannels)
                {
                    if (channel != null)
                    {
                        sampleIntervals.Add((long)Math.Round(1e9 / channel.SampleRate));
                        channelToIndex[(comp.Id, channel.Id)] = channelNames.Count;
                        channelNames.Add($"{comp.PairNumber}:{channel.Name}");
                        sampleCounts.Add(0);
                        samples.Add(new double[1]);
                        segments.Add(new ArraySegment<double>());
                    }
                }
            }
            channelsChanged = true;
        }
        EmitText($"Channels {string.Join(", ", channelNames)}");

        EmitText("Start Pipeline");
        collectionCompleteSource = new TaskCompletionSource();
        await pipeline.Start();
        collecting = true;
    }

    private async Task StopPipeline()
    {
        var pipeline = this.pipeline;
        if (pipeline == null || !collecting)
        {
            return;
        }
        EmitText("Stopping Pipeline");
        await pipeline.Stop();
        EmitText("Stopped Pipeline");
        await collectionCompleteSource.Task;
        collecting = false;
        EmitText("Collection Complete");
    }

    // Called on DelsysAPI's thread.
    private void OnData(object? sender, DelsysAPI.Events.ComponentDataReadyEventArgs e)
    {
        if (busy)
        {
            MainThread.Post(() => EmitText("Busy, Data dropped"));
            return;
        }

        for (var i = 0; i < sampleCounts.Count; ++i)
        {
            sampleCounts[i] = 0;
        }
        foreach (var frame in e.Data)
        {
            foreach (var sensor in frame.SensorData)
            {
                foreach (var channel in sensor.ChannelData)
                {
                    var i = channelToIndex[(sensor.Id, channel.Id)];
                    sampleCounts[i] += channel.Data.Count;
                }
            }
        }

        for (var i = 0; i < sampleCounts.Count; ++i)
        {
            if (samples[i].Length < sampleCounts[i])
            {
                samples[i] = new double[sampleCounts[i]];
            }
            segments[i] = new ArraySegment<double>(samples[i], 0, sampleCounts[i]);
            sampleCounts[i] = 0;
        }

        foreach (var frame in e.Data)
        {
            foreach (var sensor in frame.SensorData)
            {
                foreach (var channel in sensor.ChannelData)
                {
                    var i = channelToIndex[(sensor.Id, channel.Id)];
                    channel.Data.CopyTo(0, samples[i], sampleCounts[i], channel.Data.Count);
                    sampleCounts[i] += channel.Data.Count;
                }
            }
        }
        busy = true;
        // Wait for subscribers to read the samples before overwriting them.
        MainThread.Run(() =>
        {
            hasAnalogData = true;
            hasTextData = false;
            Ready();
            channelsChanged = false;
            busy = false;
        }).Wait();
    }

    private void EmitText(string text)
    {
        this.text = text;
        hasAnalogData = false;
        hasTextData = true;
        Ready();
    }

    public override async Task<JsonNode?> Process(JsonNode? request)
    {
        if (request is not JsonObject obj)
        {
            return new JsonObject();
        }

        if (obj["type"] is not JsonValue typeValue || !typeValue.TryGetValue<string>(out var type))
        {
            throw new InvalidDataException("Request has no type");
        }

        if (type == "get_sample_modes")
        {
            if (pipeline == null)
            {
                EmitText("Pipeline not ready");
                return new JsonObject { ["sample_modes"] = new JsonArray() };
            }
            var id = RequestId(obj);
            foreach (var component in pipeline.TrignoRfManager.Components)
            {
                if (component.PairNumber == id)
                {
                    var modes = new JsonArray();
                    foreach (var mode in component.Configuration.SampleModes)
                    {
                        modes.Add((JsonNode?)JsonValue.Create(mode));
                    }
                    return new JsonObject { ["sample_modes"] = modes };
                }
            }
            return new JsonObject { ["sample_modes"] = new JsonArray() };
        }
        else if (type == "connect")
        {
            CreateDataSource();
            return new JsonObject();
        }
        else if (type == "pair")
        {
            if (pipeline == null)
            {
                EmitText("Pipeline not ready");
                return new JsonObject();
            }
            var id = RequestId(obj);
            cancellation?.Cancel();
            cancellation = new CancellationTokenSource();
            EmitText($"Pairing component {id}");

            ThalamusHost.ShowDialog("Delsys Pairing", $"Pairing component {id}, tap to magnet", DialogType.Info);

            var success = await pipeline.TrignoRfManager.AddTrignoComponent(cancellation.Token, id);
            EmitText(success ? $"Component {id} paired" : $"Component {id} not paired");
            if (components != null)
            {
                foreach (var row in components.Values())
                {
                    if (row is State rowState && rowState["ID"] is long rowId && rowId == id)
                    {
                        rowState["Ready"] = success;
                    }
                }
            }
            return new JsonObject();
        }
        else if (type == "scan")
        {
            if (pipeline == null)
            {
                EmitText("Pipeline not ready");
                return new JsonObject();
            }
            cancellation?.Cancel();
            cancellation = new CancellationTokenSource();
            EmitText("Scanning");
            var success = await pipeline.Scan();
            if (success)
            {
                scanned = true;
                EmitText("Scan succeeded");
            }
            else
            {
                EmitText("Scan failed");
            }
            if (components != null)
            {
                foreach (var component in pipeline.TrignoRfManager.Components)
                {
                    foreach (var row in components.Values())
                    {
                        if (row is State rowState && rowState["ID"] is long rowId && rowId == component.PairNumber)
                        {
                            rowState["Ready"] = success;
                        }
                    }
                }
            }
            return new JsonObject();
        }
        throw new InvalidDataException($"Unknown request type {type}");
    }

    private static int RequestId(JsonObject request)
    {
        if (request["id"] is JsonValue idValue && idValue.TryGetValue<int>(out var id))
        {
            return id;
        }
        throw new InvalidDataException("Request has no integer id");
    }

    private void Log<T>(object? sender, T e)
    {
        MainThread.Post(() =>
        {
            EmitText($"{e}");
            Console.WriteLine(e);
        });
    }

    public int NumChannels() => channelNames.Count;

    public long SampleIntervalNs(int channel) => sampleIntervals[channel];

    public string Name(int channel) => channelNames[channel];

    public bool HasAnalogData() => hasAnalogData;

    public ReadOnlySpan<double> Data(int channel) => segments[channel];

    public bool ChannelsChanged() => channelsChanged;

    public string Text() => text;

    public bool HasTextData() => hasTextData;
}
