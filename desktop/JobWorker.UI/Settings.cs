using System.Diagnostics;
using System.Text;
using System.Text.Json;
using System.Text.RegularExpressions;

namespace JobWorker.UI;

public sealed record Settings
{
    public string ServerUrl { get; init; } = "";
    public string WorkerId { get; init; } = "windows-worker";
    public string Name { get; init; } = "Windows worker";
    public string Tags { get; init; } = "[\"os=windows\"]";
    public string WorkDir { get; init; } = Path.Combine(
        Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "JobManage", "work");
    public string NodePath { get; init; } = "node";
    public string PythonPath { get; init; } = "python";
    public string CaFile { get; init; } = "";
    public string EnvironmentAllowlist { get; init; } = "";
    public bool AllowHttp { get; init; }

    public void Validate(string token)
    {
        if (!Uri.TryCreate(ServerUrl, UriKind.Absolute, out var url) ||
            !(url.Scheme == "https" || (AllowHttp && url.Scheme == "http")) ||
            string.IsNullOrEmpty(url.Host) || url.UserInfo.Length > 0 ||
            url.Query.Length > 0 || url.Fragment.Length > 0)
            throw new ArgumentException("接続先は認証情報・クエリ・フラグメントを含まない HTTPS URL を指定してください。");
        if (string.IsNullOrWhiteSpace(token) || token.Any(c => c < 32 || c > 126))
            throw new ArgumentException("ワーカートークンを半角文字で入力してください。");
        if (!Regex.IsMatch(WorkerId, @"\A[A-Za-z0-9_.-]{1,120}\z") || WorkerId is "." or "..")
            throw new ArgumentException("ワーカー ID は半角英数字・_・-・. の 1～120 文字で指定してください（. と .. は使用不可）。");
        if (Encoding.UTF8.GetByteCount(Name) is < 1 or > 160)
            throw new ArgumentException("表示名は UTF-8 で 1～160 バイトにしてください。");
        try
        {
            var tags = JsonSerializer.Deserialize<string[]>(Tags);
            if (tags is null || tags.Length > 64 || tags.Any(t => t is null || Encoding.UTF8.GetByteCount(t) is < 1 or > 128))
                throw new JsonException();
        }
        catch (JsonException) { throw new ArgumentException("タグは文字列の JSON 配列で指定してください（最大 64 件、各 1～128 バイト）。"); }
        if (!Path.IsPathFullyQualified(WorkDir))
            throw new ArgumentException("作業フォルダーは絶対パスで指定してください。");
        if (string.IsNullOrWhiteSpace(NodePath) || string.IsNullOrWhiteSpace(PythonPath))
            throw new ArgumentException("Node / Python のコマンド名または実行ファイルを入力してください。");
        if (CaFile.Length > 0 && (!Path.IsPathFullyQualified(CaFile) || !File.Exists(CaFile)))
            throw new ArgumentException("CA 証明書は存在する PEM ファイルの絶対パスを指定してください。");
        foreach (var key in EnvironmentAllowlist.Split(',', StringSplitOptions.TrimEntries | StringSplitOptions.RemoveEmptyEntries))
            if (key.StartsWith("JOB_", StringComparison.OrdinalIgnoreCase) || !Regex.IsMatch(key, @"\A[A-Za-z0-9_]+\z"))
                throw new ArgumentException("追加環境変数には変数名をカンマ区切りで指定してください。JOB_ は許可できません。");
    }

    public ProcessStartInfo StartInfo(string executable, string token)
    {
        Validate(token);
        var info = new ProcessStartInfo(executable)
        {
            UseShellExecute = false, CreateNoWindow = true,
            RedirectStandardInput = true, RedirectStandardOutput = true, RedirectStandardError = true,
            StandardOutputEncoding = Encoding.UTF8, StandardErrorEncoding = Encoding.UTF8,
            WorkingDirectory = AppContext.BaseDirectory
        };
        info.ArgumentList.Add("--managed");
        // Do not let inherited JOB_* settings silently override the form.
        foreach (var key in info.Environment.Keys.Where(k => k.StartsWith("JOB_", StringComparison.OrdinalIgnoreCase)).ToArray())
            info.Environment.Remove(key);
        info.Environment["JOB_SERVER_URL"] = ServerUrl;
        info.Environment["JOB_WORKER_TOKEN"] = token;
        info.Environment["JOB_WORKER_ID"] = WorkerId;
        info.Environment["JOB_WORKER_NAME"] = Name;
        info.Environment["JOB_WORKER_TAGS"] = Tags;
        info.Environment["JOB_WORK_DIR"] = WorkDir;
        info.Environment["JOB_NODE_PATH"] = NodePath;
        info.Environment["JOB_PYTHON_PATH"] = PythonPath;
        info.Environment["JOB_ALLOW_INSECURE_HTTP"] = AllowHttp ? "true" : "false";
        info.Environment["JOB_RUNTIME_ENV_ALLOWLIST"] = EnvironmentAllowlist;
        if (CaFile.Length > 0) info.Environment["JOB_CA_CERT_FILE"] = CaFile;
        info.Environment["RUST_LOG"] = "job_worker=info";
        return info;
    }

    public static Settings Load(string path) => File.Exists(path)
        ? JsonSerializer.Deserialize<Settings>(File.ReadAllText(path)) ?? throw new JsonException("設定が空です。")
        : new();

    public void Save(string path)
    {
        Directory.CreateDirectory(Path.GetDirectoryName(path)!);
        var temporary = path + ".tmp";
        File.WriteAllText(temporary, JsonSerializer.Serialize(this, new JsonSerializerOptions { WriteIndented = true }));
        File.Move(temporary, path, overwrite: true);
    }
}
