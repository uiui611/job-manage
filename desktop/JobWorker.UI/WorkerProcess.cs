using System.Diagnostics;

namespace JobWorker.UI;

// All public methods are called from the UI thread; pipe readers only touch the log lock.
public sealed class WorkerProcess
{
    private readonly Queue<string> chunks = new();
    private readonly object logLock = new();
    private Process? process;
    private Task[] readers = [];
    private bool omitted;
    public bool IsRunning => process is { HasExited: false };
    public bool HasProcess => process is not null;
    public int? ExitCode => process is { HasExited: true } p ? p.ExitCode : null;

    public void Start(Settings settings, string token, string executable)
    {
        if (process is not null) throw new InvalidOperationException("前のワーカーの終了処理を待ってください。");
        var child = new Process { StartInfo = settings.StartInfo(executable, token) };
        try
        {
            if (!child.Start()) throw new InvalidOperationException("ワーカーを起動できませんでした。");
            // Release the extra token copy in ProcessStartInfo after CreateProcess.
            child.StartInfo.Environment.Remove("JOB_WORKER_TOKEN");
            process = child;
            readers = [ReadLog(child.StandardOutput, token), ReadLog(child.StandardError, token)];
        }
        catch { child.Dispose(); throw; }
    }

    private async Task ReadLog(StreamReader reader, string token)
    {
        // Bounded even for a stream without newlines. Keep a tail to redact tokens
        // spanning reads, without buffering arbitrary log lines.
        var buffer = new char[2048];
        var tail = "";
        try
        {
            int count;
            while ((count = await reader.ReadAsync(buffer).ConfigureAwait(false)) > 0)
            {
                var text = tail + new string(buffer, 0, count);
                var end = Math.Max(0, text.Length - token.Length + 1);
                var match = text.LastIndexOf(token, StringComparison.Ordinal);
                if (match >= 0 && match < end && match + token.Length > end) end = match;
                Enqueue(text[..end].Replace(token, "[REDACTED]", StringComparison.Ordinal));
                tail = text[end..];
            }
            Enqueue(tail.Replace(token, "[REDACTED]", StringComparison.Ordinal));
        }
        catch (IOException) { Enqueue("\nログストリームが閉じられました。\n"); }
    }

    private void Enqueue(string text)
    {
        lock (logLock)
        {
            if (chunks.Count == 64) { chunks.Dequeue(); omitted = true; }
            chunks.Enqueue(text);
        }
    }

    public string DrainLogs()
    {
        lock (logLock)
        {
            var text = (omitted ? "\n[大量のログの一部を省略しました]\n" : "") + string.Concat(chunks);
            chunks.Clear();
            omitted = false;
            return text;
        }
    }

    public async Task StopAsync()
    {
        if (process is not { } child) return;
        if (!child.HasExited)
        {
            // EOF requests the same cancellation/cleanup path as Ctrl+C.
            try { child.StandardInput.Close(); } catch (IOException) { }
            try { await child.WaitForExitAsync().WaitAsync(TimeSpan.FromSeconds(15)); }
            catch (TimeoutException)
            {
                Enqueue("\n停止待ちが 15 秒を超えたため強制終了します。\n");
                try { child.Kill(entireProcessTree: true); }
                catch (InvalidOperationException) when (child.HasExited) { }
                await child.WaitForExitAsync();
            }
        }
        await ReapAsync();
    }

    public async Task ReapAsync()
    {
        if (process is not { HasExited: true } child) return;
        await Task.WhenAll(readers);
        Enqueue($"\nワーカー終了（終了コード {child.ExitCode}）\n");
        child.Dispose();
        process = null;
        readers = [];
    }
}
