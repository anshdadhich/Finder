using System.Diagnostics;
using System.IO.Compression;
using System.Text.Json;
using System.Threading.Channels;
using FastSearch.Index;
using FastSearch.Mft;
using FastSearch.Utils;

Console.WriteLine("╔══════════════════════════════════╗");
Console.WriteLine("║       FastSeek - File Search      ║");
Console.WriteLine("╚══════════════════════════════════╝");
Console.WriteLine();

var drives = Drives.GetNtfsDrives();
if (drives.Count == 0)
{
    Console.Error.WriteLine("No NTFS drives found. Are you running as Administrator?");
    Environment.Exit(1);
}

var index = new LockedIndex(new IndexStore());
var channel = Channel.CreateUnbounded<IndexEvent>();
var cachePath = Path.Combine(Path.GetTempPath(), "fastseek_cache.bin");
var cacheLoaded = false;

if (File.Exists(cachePath))
{
    Console.Write("Loading cached index... ");
    try
    {
        await using var file = File.OpenRead(cachePath);
        await using var gzip = new GZipStream(file, CompressionMode.Decompress);
        var cache = await JsonSerializer.DeserializeAsync<CacheData>(gzip);
        if (cache is null) throw new InvalidDataException("cache empty");

        var count = cache.Entries.Count;
        var checkpoints = cache.Checkpoints.ToList();
        index.Write(store => IndexStore.FromCache(cache));
        Console.WriteLine($"{count} files");

        if (checkpoints.Count != 0)
        {
            Console.Write("Catching up on changes since last run... ");
            var delta = Channel.CreateUnbounded<IndexEvent>();
            var journalOk = true;

            foreach (var drive in drives)
            {
                var cp = checkpoints.FirstOrDefault(c => c.DriveLetter == drive.Letter);
                if (cp is null)
                {
                    Console.WriteLine($"missing checkpoint for {drive.Letter}:, full rescan needed.");
                    File.Delete(cachePath);
                    journalOk = false;
                    break;
                }

                try
                {
                    using var watcher = UsnWatcher.NewFrom(drive, delta.Writer, cp);
                    watcher.Drain();
                    var newCp = watcher.Checkpoint();
                    index.WithWrite(store =>
                    {
                        store.Checkpoints.RemoveAll(c => c.DriveLetter == drive.Letter);
                        store.Checkpoints.Add(newCp);
                    });
                }
                catch
                {
                    Console.WriteLine("journal reset, full rescan needed.");
                    File.Delete(cachePath);
                    journalOk = false;
                    break;
                }
            }

            delta.Writer.Complete();
            if (journalOk)
            {
                var applied = 0;
                await foreach (var ev in delta.Reader.ReadAllAsync())
                {
                    index.WithWrite(store => ApplyEvent(store, ev));
                    applied++;
                }
                Console.WriteLine($"{applied} change(s) applied");
                Console.WriteLine();
                cacheLoaded = true;
            }
        }
        else
        {
            Console.WriteLine();
            cacheLoaded = true;
        }
    }
    catch
    {
        Console.WriteLine("cache corrupt, rescanning...");
    }
}

if (!cacheLoaded)
{
    Console.WriteLine($"Found drives: {string.Join(", ", drives.Select(d => $"{d.Letter}:"))}");
    Console.WriteLine("Building index...");

    var totalStart = Stopwatch.StartNew();

    index.WithWrite(store =>
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

    var total = 0;
    var totalScanTime = TimeSpan.Zero;
    var totalIndexTime = TimeSpan.Zero;

    foreach (var drive in drives)
    {
        Console.Write($"  Scanning {drive.Letter}:  ... ");
        try
        {
            using var reader = MftReader.Open(drive);
            var t1 = Stopwatch.StartNew();
            var direct = reader.ScanDirect();
            var scan = direct ?? reader.Scan();
            var method = direct is null ? "ioctl" : "direct";
            t1.Stop();

            var t2 = Stopwatch.StartNew();
            index.WithWrite(store => store.PopulateFromScan(scan, drive.Root));
            t2.Stop();

            Console.WriteLine($"{scan.Records.Count} files  (scan {t1.Elapsed.TotalSeconds:F2}s {method}, index {t2.Elapsed.TotalSeconds:F2}s)");
            total += scan.Records.Count;
            totalScanTime += t1.Elapsed;
            totalIndexTime += t2.Elapsed;
        }
        catch (Exception e)
        {
            Console.WriteLine($"FAILED ({e.Message})");
        }
    }

    index.WithWrite(store => store.FinalizeIndex());
    Console.WriteLine();
    Console.WriteLine($"Index ready - {total} total files  (scan {totalScanTime.TotalSeconds:F2}s, index {totalIndexTime.TotalSeconds:F2}s)");

    await SaveCache(index, cachePath);
    Console.WriteLine($"Total startup: {totalStart.Elapsed.TotalSeconds:F2}s");
    Console.WriteLine();
}

var liveCheckpoints = index.Read(store => store.Checkpoints.ToList());
var checkpointGate = new object();

foreach (var drive in drives)
{
    var tx = channel.Writer;
    _ = Task.Run(() =>
    {
        try
        {
            using var watcher = UsnWatcher.New(drive, tx);
            watcher.RunShared(liveCheckpoints, checkpointGate);
        }
        catch { }
    });
}

_ = Task.Run(async () =>
{
    await foreach (var ev in channel.Reader.ReadAllAsync())
    {
        index.WithWrite(store => ApplyEvent(store, ev));
    }
});

Console.CancelKeyPress += (_, args) =>
{
    args.Cancel = true;
    lock (checkpointGate)
    {
        index.WithWrite(store => store.Checkpoints = liveCheckpoints.ToList());
    }
    SaveCache(index, cachePath).GetAwaiter().GetResult();
    Environment.Exit(0);
};

SearchLoop(index);

static void SearchLoop(LockedIndex index)
{
    var configPath = Path.Combine(ConfigDir(), "config.txt");
    var caseSensitive = false;
    var excludedDirs = LoadExclusions(configPath);

    Console.WriteLine("Commands:");
    Console.WriteLine("  <query>           search files");
    Console.WriteLine("  folder:<query>    directories only    (or :<query>)");
    Console.WriteLine("  file:<query>      files only          (or !<query>)");
    Console.WriteLine("  *.ext / ext:ext   by extension e.g. *.pdf, ext:docx");
    Console.WriteLine("  case              toggle case sensitivity [off]");
    Console.WriteLine("  exclude <path>    exclude a directory");
    Console.WriteLine("  unexclude <path>  remove exclusion");
    Console.WriteLine("  exclusions        list excluded dirs");
    Console.WriteLine("  count             total indexed files");
    Console.WriteLine("  rescan            clear cache and rescan");
    Console.WriteLine("  quit              exit");
    Console.WriteLine();

    while (true)
    {
        Console.Write("search> ");
        var input = Console.ReadLine()?.Trim();
        if (string.IsNullOrEmpty(input)) continue;

        switch (input)
        {
            case "quit":
            case "exit":
            case "q":
                Console.WriteLine("Bye.");
                return;
            case "count":
                Console.WriteLine($"  {index.Read(store => store.Len())} files in index\n");
                continue;
            case "rescan":
                File.Delete(Path.Combine(Path.GetTempPath(), "fastseek_cache.bin"));
                Console.WriteLine("Cache cleared. Restart fastseek to rescan.\n");
                continue;
            case "case":
                caseSensitive = !caseSensitive;
                Console.WriteLine($"  case sensitivity: {(caseSensitive ? "ON" : "OFF")}\n");
                continue;
            case "exclusions":
                if (excludedDirs.Count == 0) Console.WriteLine("  no excluded directories\n");
                else
                {
                    Console.WriteLine();
                    foreach (var d in excludedDirs) Console.WriteLine($"  - {d}");
                    Console.WriteLine();
                }
                continue;
        }

        if (input.StartsWith("exclude ", StringComparison.Ordinal))
        {
            var path = NormalizeExcludedPath(input[8..]);
            if (!excludedDirs.Contains(path))
            {
                excludedDirs.Add(path);
                SaveExclusions(configPath, excludedDirs);
            }
            Console.WriteLine($"  excluded: {path}\n");
            continue;
        }

        if (input.StartsWith("unexclude ", StringComparison.Ordinal))
        {
            var path = NormalizeExcludedPath(input[10..]);
            var removed = excludedDirs.Remove(path);
            SaveExclusions(configPath, excludedDirs);
            Console.WriteLine(removed ? $"  removed: {path}\n" : "  not found in exclusions\n");
            continue;
        }

        var parsed = ParseQuery(input);
        var start = Stopwatch.StartNew();
        var results = index.Read(store =>
        {
            if (parsed.ExtFilter is not null)
            {
                var dotExt = "." + parsed.ExtFilter;
                return store.Entries
                    .Select(entry =>
                    {
                        var name = store.NameLower(entry);
                        if (!name.EndsWith(dotExt, StringComparison.Ordinal)) return null;
                        var kindOk = parsed.Filter switch
                        {
                            Filter.All => true,
                            Filter.Dirs => entry.Kind() == FileKind.Directory,
                            Filter.Files => entry.Kind() != FileKind.Directory,
                            _ => true,
                        };
                        if (!kindOk) return null;
                        var fullPath = Searcher.BuildPath(entry.FileRef, store);
                        if (excludedDirs.Count != 0 && excludedDirs.Any(fullPath.ToLowerInvariant().StartsWith)) return null;
                        return new SearchResult(fullPath, store.Name(entry), 0, entry.Kind() == FileKind.Directory);
                    })
                    .Where(r => r is not null)
                    .Take(50)
                    .Cast<SearchResult>()
                    .ToList();
            }

            return Searcher.Search(store, parsed.Query, 200, caseSensitive, excludedDirs)
                .Where(r => parsed.Filter switch
                {
                    Filter.All => true,
                    Filter.Dirs => r.IsDir,
                    Filter.Files => !r.IsDir,
                    _ => true,
                })
                .Take(50)
                .ToList();
        });
        start.Stop();

        if (results.Count == 0)
        {
            Console.WriteLine($"  no results for \"{input}\"\n");
        }
        else
        {
            Console.WriteLine();
            for (var i = 0; i < results.Count; i++)
            {
                var kind = results[i].IsDir ? "DIR " : "FILE";
                Console.WriteLine($"  [{i + 1,3}] [{kind}] {results[i].FullPath}");
            }
            Console.WriteLine();
            Console.WriteLine($"  {results.Count} result(s) in {start.Elapsed.TotalMilliseconds:F2}ms\n");
        }
    }
}

static ParsedQuery ParseQuery(string input)
{
    if (input.StartsWith("ext:", StringComparison.Ordinal)) return new("", Filter.Files, input[4..].ToLowerInvariant());
    if (input.StartsWith("*.", StringComparison.Ordinal)) return new("", Filter.All, input[2..].ToLowerInvariant());
    if (input.StartsWith("folder:", StringComparison.Ordinal)) return new(input[7..].Trim(), Filter.Dirs, null);
    if (input.StartsWith("file:", StringComparison.Ordinal)) return new(input[5..].Trim(), Filter.Files, null);
    if (input.StartsWith(':')) return new(input[1..], Filter.Dirs, null);
    if (input.StartsWith('!')) return new(input[1..], Filter.Files, null);
    return new(input, Filter.All, null);
}

static string ConfigDir()
{
    var dir = Path.Combine(Environment.GetEnvironmentVariable("APPDATA") ?? Path.GetTempPath(), "fastsearch");
    Directory.CreateDirectory(dir);
    return dir;
}

static List<string> LoadExclusions(string path) =>
    File.Exists(path)
        ? File.ReadAllLines(path).Select(l => l.Trim().ToLowerInvariant()).Where(l => l.Length != 0).ToList()
        : [];

static void SaveExclusions(string path, List<string> dirs) => File.WriteAllText(path, string.Join('\n', dirs));

static string NormalizeExcludedPath(string input)
{
    var path = input.Trim().ToLowerInvariant();
    return path.EndsWith('\\') || path.EndsWith('/') ? path : path + "\\";
}

static void ApplyEvent(IndexStore store, IndexEvent ev)
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

static async Task SaveCache(LockedIndex index, string cachePath)
{
    var cache = index.Read(store => store.ToCache());
    await using var file = File.Create(cachePath);
    await using var gzip = new GZipStream(file, CompressionLevel.Fastest);
    await JsonSerializer.SerializeAsync(gzip, cache);
    var rawMb = JsonSerializer.SerializeToUtf8Bytes(cache).Length / 1_048_576.0;
    var compMb = new FileInfo(cachePath).Length / 1_048_576.0;
    Console.WriteLine($"Cache saved - {compMb:F1}MB compressed ({rawMb:F1}MB raw)");
}

internal enum Filter { All, Dirs, Files }
internal sealed record ParsedQuery(string Query, Filter Filter, string? ExtFilter);

internal sealed class LockedIndex(IndexStore initial)
{
    private readonly ReaderWriterLockSlim _lock = new();
    private IndexStore _store = initial;

    public T Read<T>(Func<IndexStore, T> action)
    {
        _lock.EnterReadLock();
        try { return action(_store); }
        finally { _lock.ExitReadLock(); }
    }

    public void WithWrite(Action<IndexStore> action)
    {
        _lock.EnterWriteLock();
        try { action(_store); }
        finally { _lock.ExitWriteLock(); }
    }

    public void Write(Func<IndexStore, IndexStore> action)
    {
        _lock.EnterWriteLock();
        try { _store = action(_store); }
        finally { _lock.ExitWriteLock(); }
    }
}
