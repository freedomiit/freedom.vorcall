using System.Net;
using System.Net.Http.Headers;
using System.Text.RegularExpressions;
using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.Hosting;
using Vorcall.Server.Diagnostics;
using Vorcall.Server.Tests.Infrastructure;
using Xunit;

namespace Vorcall.Server.Tests;

[Collection(ServerCollection.Name)]
public sealed class DiagnosticsTests(ServerFixture fixture)
{
    private const string TextType = "text/plain; charset=utf-8";
    private const string TooLargeDetail = "reports must be 4 MiB or smaller";
    private const int MaxBytes = 4 << 20;

    private VorcallFactory Server => fixture.Server;

    [Fact]
    public async Task Upload_stores_the_report_under_the_expected_name()
    {
        var alice = await Accounts.RegisterAsync(Server, "alice");
        var body = "line one\nline two\n"u8.ToArray();

        var log = await UploadAsync(Server, alice.Access, "log", "vorcall.log", body);
        Assert.Equal(HttpStatusCode.NoContent, log.Status);
        var stored = StoredFile(Server, alice, "log", "vorcall.log");
        Assert.Equal(body, await File.ReadAllBytesAsync(stored));

        var crash = await UploadAsync(Server, alice.Access, "crash", "crash-1.txt", body);
        Assert.Equal(HttpStatusCode.NoContent, crash.Status);
        StoredFile(Server, alice, "crash", "crash-1.txt");
    }

    [Fact]
    public async Task Upload_refuses_binary_bodies_bad_names_and_unknown_kinds()
    {
        var alice = await Accounts.RegisterAsync(Server, "alice");
        var text = "fine\n"u8.ToArray();

        foreach (var binary in new[] { new byte[] { (byte)'a', 0x00, (byte)'b' }, new byte[] { 0xFF, 0xFE } })
        {
            var response = await UploadAsync(Server, alice.Access, "log", "vorcall.log", binary);
            Assert.Equal(HttpStatusCode.BadRequest, response.Status);
            Assert.Equal("not text", response.Detail);
        }

        foreach (var name in new string?[] { ".hidden", "../evil.log", null })
        {
            var response = await UploadAsync(Server, alice.Access, "log", name, text);
            Assert.Equal(HttpStatusCode.BadRequest, response.Status);
            Assert.Equal("invalid file name", response.Detail);
        }

        var kind = await UploadAsync(Server, alice.Access, "oops", "vorcall.log", text);
        Assert.Equal(HttpStatusCode.BadRequest, kind.Status);
        Assert.Equal("invalid kind", kind.Detail);
    }

    [Fact]
    public async Task Upload_over_4_MiB_answers_413_from_the_header_alone_and_from_the_body()
    {
        var alice = await Accounts.RegisterAsync(Server, "alice");

        var declared = await UploadAsync(Server, alice.Access, "log", "big.log", "tiny\n"u8.ToArray(), declaredLength: MaxBytes + 1);
        Assert.Equal(HttpStatusCode.RequestEntityTooLarge, declared.Status);
        Assert.Equal(TooLargeDetail, declared.Detail);

        // No declared length at all: the store has to count the bytes itself.
        var oversized = new byte[MaxBytes + 1];
        Array.Fill(oversized, (byte)'a');
        var streamed = await UploadAsync(Server, alice.Access, "log", "big.log", oversized, sized: false);
        Assert.Equal(HttpStatusCode.RequestEntityTooLarge, streamed.Status);
        Assert.Equal(TooLargeDetail, streamed.Detail);
    }

    [Fact]
    public async Task Upload_past_the_hourly_limit_answers_429()
    {
        var server = await fixture.TightLimitsAsync();
        var alice = await Accounts.RegisterAsync(server, "alice");
        var body = "report\n"u8.ToArray();

        for (var i = 0; i < ServerFixture.TightDiagnosticsPerHour; i++)
        {
            Assert.Equal(HttpStatusCode.NoContent, (await UploadAsync(server, alice.Access, "log", $"report-{i}.log", body)).Status);
        }

        var refused = await UploadAsync(server, alice.Access, "log", "report-late.log", body);
        Assert.Equal(HttpStatusCode.TooManyRequests, refused.Status);
        Assert.Equal("too many requests", refused.Detail);
        Assert.Equal("60", refused.Header("Retry-After"));
    }

    [Fact]
    public async Task The_sweeper_deletes_reports_older_than_the_retention_and_keeps_the_rest()
    {
        var alice = await Accounts.RegisterAsync(Server, "alice");
        var body = "report\n"u8.ToArray();

        Assert.Equal(HttpStatusCode.NoContent, (await UploadAsync(Server, alice.Access, "log", "old.log", body)).Status);
        var old = StoredFile(Server, alice, "log", "old.log");
        File.SetLastWriteTimeUtc(old, DateTime.UtcNow - DiagnosticsOptions.Retention - TimeSpan.FromDays(1));

        Assert.Equal(HttpStatusCode.NoContent, (await UploadAsync(Server, alice.Access, "log", "new.log", body)).Status);
        var fresh = StoredFile(Server, alice, "log", "new.log");

        var sweeper = Server.Services.GetServices<IHostedService>().OfType<DiagnosticsSweeper>().Single();
        Assert.True(sweeper.Sweep(DateTime.UtcNow) >= 1);
        Assert.False(File.Exists(old));
        Assert.True(File.Exists(fresh));
    }

    private static Task<ProtoResponse> UploadAsync(
        VorcallFactory server,
        string bearer,
        string kind,
        string? fileName,
        byte[] body,
        long? declaredLength = null,
        bool sized = true)
    {
        HttpContent content = sized ? new ByteArrayContent(body) : new UnsizedContent(body);
        content.Headers.ContentType = MediaTypeHeaderValue.Parse(TextType);
        if (declaredLength is { } length)
        {
            content.Headers.ContentLength = length;
        }

        return Proto.SendAsync(
            server,
            HttpMethod.Post,
            $"/api/diagnostics?kind={Uri.EscapeDataString(kind)}",
            content,
            bearer,
            ServerFixture.ServerKey,
            request =>
            {
                if (fileName is not null)
                {
                    request.Headers.Add("X-Vorcall-Filename", fileName);
                }
            });
    }

    // The one file in the diagnostics directory stored for that account, kind and name.
    private static string StoredFile(VorcallFactory server, Account account, string kind, string name)
    {
        var pattern = $"^{account.UserId}-{Regex.Escape(account.Username)}-\\d{{8}}T\\d{{6}}Z-{kind}-{Regex.Escape(name)}$";
        var matches = Directory.GetFiles(server.DiagnosticsDir)
            .Where(path => Regex.IsMatch(Path.GetFileName(path), pattern))
            .ToArray();
        Assert.True(matches.Length == 1, $"{matches.Length} files match {pattern} in {server.DiagnosticsDir}");
        return matches[0];
    }

    // A body without a Content-Length: the request goes out chunked, so the store is what
    // discovers the size.
    private sealed class UnsizedContent(byte[] payload) : HttpContent
    {
        protected override Task SerializeToStreamAsync(Stream stream, TransportContext? context)
            => stream.WriteAsync(payload).AsTask();

        protected override bool TryComputeLength(out long length)
        {
            length = 0;
            return false;
        }
    }
}
