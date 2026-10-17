using JobWorker.UI;

namespace JobWorker.UI.Tests;

internal static class Program
{
    [STAThread]
    private static int Main(string[] args)
    {
        if (args.Contains("--managed"))
        {
            Console.OutputEncoding = System.Text.Encoding.UTF8;
            var token = Environment.GetEnvironmentVariable("JOB_WORKER_TOKEN")!;
            Console.Write(token[..3]);
            Console.Out.Flush();
            Thread.Sleep(100);
            Console.WriteLine(token[3..]);
            Console.WriteLine("日本語ログ");
            Console.In.ReadToEnd();
            Console.WriteLine("graceful shutdown");
            return 0;
        }
        try { RunTests(args).GetAwaiter().GetResult(); return 0; }
        catch (Exception ex) { Console.Error.WriteLine(ex); return 1; }
    }

    private static async Task RunTests(string[] args)
    {
        Application.SetHighDpiMode(HighDpiMode.PerMonitorV2);
        Application.EnableVisualStyles();
        Application.SetCompatibleTextRenderingDefault(false);
        using (var form = new MainForm())
        {
            form.Show();
            Application.DoEvents();
            Check(form.Text.Contains("Windows Worker"), "desktop form opens");
            using var bitmap = new Bitmap(form.Width, form.Height);
            form.DrawToBitmap(bitmap, form.ClientRectangle);
            bitmap.Save(Path.Combine(AppContext.BaseDirectory, "ui-smoke.png"));
            form.Close();
        }
        // This console harness has no persistent WinForms message loop.
        SynchronizationContext.SetSynchronizationContext(null);
        string root = Path.Combine(Path.GetTempPath(), "job-ui-test-" + Guid.NewGuid());
        Directory.CreateDirectory(root);
        try
        {
            var settings = new Settings { ServerUrl = "https://example.invalid/jobs", WorkDir = root };
            const string token = "test-only-token";
            settings.Validate(token);
            var cases = System.Text.Json.JsonSerializer.Deserialize<WorkerIdCase[]>(
                File.ReadAllText(Path.Combine(AppContext.BaseDirectory, "worker-id-cases.json")))!;
            foreach (var item in cases)
            {
                var accepted = true;
                try { (settings with { WorkerId = item.id }).Validate(token); }
                catch (ArgumentException) { accepted = false; }
                Check(accepted == item.valid, $"shared worker ID contract: {item.id}");
            }
            var path = Path.Combine(root, "settings.json");
            settings.Save(path);
            Check(Settings.Load(path) == settings, "settings round trip");
            Check(!File.ReadAllText(path).Contains(token), "token must not be persisted");
            foreach (var invalid in new[] {
                settings with { ServerUrl = "http://example.invalid" },
                settings with { ServerUrl = "https://user:pass@example.invalid" },
                settings with { WorkerId = ".." }, settings with { Tags = "[null]" },
                settings with { Tags = "null" }, settings with { WorkDir = "relative" },
                settings with { EnvironmentAllowlist = "job_worker_token" } })
            {
                try { invalid.Validate(token); throw new Exception("invalid setting accepted"); }
                catch (ArgumentException) { }
            }
            Environment.SetEnvironmentVariable("JOB_CA_CERT_FILE", "inherited-invalid-value");
            var info = settings.StartInfo("worker.exe", token);
            Check(!info.Environment.ContainsKey("JOB_CA_CERT_FILE"), "clear inherited settings");
            Check(info.ArgumentList.SequenceEqual(new[] { "--managed" }), "no secrets in arguments");
            Environment.SetEnvironmentVariable("JOB_CA_CERT_FILE", null);

            // A fake worker exercises UTF-8 output, split-token redaction, EOF shutdown,
            // process reaping and starting again, without a server or any credentials.
            var worker = new WorkerProcess();
            for (int attempt = 0; attempt < 2; attempt++)
            {
                worker.Start(settings, token, Environment.ProcessPath!);
                await Task.Delay(300);
                await worker.StopAsync();
                var log = worker.DrainLogs();
                Check(log.Contains("graceful shutdown") && log.Contains("日本語ログ"), "EOF and UTF-8");
                Check(!log.Contains(token) && log.Contains("[REDACTED]"), "redaction");
                Check(!worker.HasProcess, "reaped");
            }

            if (args.Length == 1)
            {
                var native = settings with { ServerUrl = "http://127.0.0.1:1", AllowHttp = true };
                worker.Start(native, token, Path.GetFullPath(args[0]));
                await Task.Delay(1500);
                Check(worker.IsRunning, "native worker must wait/reconnect");
                await worker.StopAsync();
                var log = worker.DrainLogs();
                Check(log.Contains("終了コード 0"), "native worker must gracefully exit on EOF");
                Check(!log.Contains("強制終了"), "no forced termination");
            }
            Console.WriteLine("Windows UI settings and process lifecycle checks passed.");
        }
        finally { Directory.Delete(root, recursive: true); }
    }

    private static void Check(bool condition, string message)
    {
        if (!condition) throw new Exception(message);
    }

    private sealed record WorkerIdCase(string id, bool valid);
}
