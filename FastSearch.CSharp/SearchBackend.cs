using System.Diagnostics;
using System.IO;
using System.Threading.Channels;
using FastSearch.Index;
using FastSearch.Mft;
using FastSearch.Utils;

namespace FastSearch;

public sealed class SearchBackend : IAsyncDisposable
{
    private readonly ReaderWriterLockSlim _lock = new();
    private readonly Channel<IndexEvent> _channel = Channel.CreateUnbounded<IndexEvent>();
    private readonly object _checkpointGate = new();
    private readonly string _cachePath = Path.Combine(Path.GetTempPath(), "fastseek_csharp_cache.bin");
    private IndexStore _store = new();
    private List<JournalCheckpoint> _liveCheckpoints = [];
    private bool _disposed;

    public bool Ready { get; private set; }
    public int Count => Read(s => s.Len());
    public string StatusText { get; private set; } = "Starting...";

    public event Action? StatusChanged;

    public async Task StartAsync()
    {
        await Task.Run(() =>
        {
            var drives = Drives.GetNtfsDrives();
            if (drives.Count == 0)
            {
                StatusText = "No NTFS drives found. Run as Administrator.";
                StatusChanged?.Invoke();
                return;
            }

            StatusText = "Loading index...";
            StatusChanged?.Invoke();

            if (!LoadCacheAndCatchUp(drives))
            {
                BuildFullIndex(drives);
            }

            Ready = true;
            StatusText = $"{Count:N0} files indexed";
            StatusChanged?.Invoke();

            _liveCheckpoints = Read(s => s.Checkpoints.ToList());
            StartWatchers(drives);
            _ = Task.Run(ProcessEventsAsync);
        });
    }

    public List<SearchResult> Search(string query, int limit = 50)
    {
        if (!Ready || string.IsNullOrWhiteSpace(query)) return [];
        return Read(store => Searcher.Search(store, query.Trim(), limit, false, []));
    }

    public void Open(string path)
    {
        Process.Start(new ProcessStartInfo(path) { UseShellExecute = true });
    }

    public void OpenParent(string path)
    {
        var parent = Path.GetDirectoryName(path);
        if (!string.IsNullOrWhiteSpace(parent))
        {
            Process.Start(new ProcessStartInfo(parent) { UseShellExecute = true });
        }
    }

    private bool LoadCacheAndCatchUp(List<NtfsDrive> drives)
    {
        if (!File.Exists(_cachePath)) return false;

        try
        {
            var cache = CacheStore.Load(_cachePath);
            var checkpoints = cache.Checkpoints.ToList();
            Write(_ => IndexStore.FromCache(cache));

            if (checkpoints.Count == 0) return true;

            var delta = Channel.CreateUnbounded<IndexEvent>();
            foreach (var drive in drives)
            {
                var cp = checkpoints.FirstOrDefault(c => c.DriveLetter == drive.Letter);
                if (cp is null)
                {
                    File.Delete(_cachePath);
                    return false;
                }

                using var watcher = UsnWatcher.NewFrom(drive, delta.Writer, cp);
                watcher.Drain();
                var newCp = watcher.Checkpoint();
                WithWrite(store =>
                {
                    store.Checkpoints.RemoveAll(c => c.DriveLetter == drive.Letter);
                    store.Checkpoints.Add(newCp);
                });
            }

            delta.Writer.Complete();
            while (delta.Reader.TryRead(out var ev))
            {
                WithWrite(store => ApplyEvent(store, ev));
            }

            return true;
        }
        catch
        {
            try { File.Delete(_cachePath); } catch { }
            return false;
        }
    }

    private void BuildFullIndex(List<NtfsDrive> drives)
    {
        StatusText = "Building index...";
        StatusChanged?.Invoke();

        WithWrite(store =>
        {
            foreach (var drive in drives)
            {
                var dummy = Channel.CreateUnbounded<IndexEvent>();
                try
                {
                    using var watcher = UsnWatcher.New(drive, dummy.Writer);
                    store.Checkpoints.Add(watcher.Checkpoint());
                }
                catch { }
            }
        });

        foreach (var drive in drives)
        {
            StatusText = $"Scanning {drive.Letter}:...";
            StatusChanged?.Invoke();

            try
            {
                using var reader = MftReader.Open(drive);
                var scan = reader.ScanDirect() ?? reader.Scan();
                WithWrite(store => store.PopulateFromScan(scan, drive.Root));
            }
            catch { }
        }

        WithWrite(store => store.FinalizeIndex());
        SaveCache();
    }

    private void StartWatchers(List<NtfsDrive> drives)
    {
        foreach (var drive in drives)
        {
            var tx = _channel.Writer;
            _ = Task.Run(() =>
            {
                try
                {
                    using var watcher = UsnWatcher.New(drive, tx);
                    watcher.RunShared(_liveCheckpoints, _checkpointGate);
                }
                catch { }
            });
        }
    }

    private async Task ProcessEventsAsync()
    {
        await foreach (var ev in _channel.Reader.ReadAllAsync())
        {
            WithWrite(store => ApplyEvent(store, ev));
        }
    }

    private void SaveCache()
    {
        lock (_checkpointGate)
        {
            WithWrite(store => store.Checkpoints = _liveCheckpoints.ToList());
        }

        var cache = Read(store => store.ToCache());
        CacheStore.SaveAtomic(_cachePath, cache);
    }

    private static void ApplyEvent(IndexStore store, IndexEvent ev)
    {
        switch (ev)
        {
            case IndexEvent.Created created:
                store.Insert(created.Record);
                break;
            case IndexEvent.Deleted deleted:
                store.Remove(deleted.FileRef);
                break;
            case IndexEvent.Renamed renamed:
                store.Rename(renamed.OldRef, renamed.NewRecord);
                break;
            case IndexEvent.Moved moved:
                store.ApplyMove(moved.FileRef, moved.NewParentRef, moved.Name, moved.Kind);
                break;
        }
    }

    private T Read<T>(Func<IndexStore, T> action)
    {
        _lock.EnterReadLock();
        try { return action(_store); }
        finally { _lock.ExitReadLock(); }
    }

    private void WithWrite(Action<IndexStore> action)
    {
        _lock.EnterWriteLock();
        try { action(_store); }
        finally { _lock.ExitWriteLock(); }
    }

    private void Write(Func<IndexStore, IndexStore> action)
    {
        _lock.EnterWriteLock();
        try { _store = action(_store); }
        finally { _lock.ExitWriteLock(); }
    }

    public ValueTask DisposeAsync()
    {
        if (!_disposed)
        {
            _disposed = true;
            SaveCache();
            _channel.Writer.TryComplete();
        }

        return ValueTask.CompletedTask;
    }
}
