using Microsoft.AspNetCore.Http.Features;
using Vorcall.Server.Auth;
using Vorcall.Server.Diagnostics;

namespace Vorcall.Server.Api;

// Under /api so the pre-shared key middleware covers it, and behind a bearer on top. The body
// streams straight into the store: nothing here holds a report in memory, and no failure of any
// kind leaves this endpoint as an exception — a client that cannot send its logs must still get
// an answer it can show the user.
public static class DiagnosticsEndpoints
{
    public const string RateLimitPolicy = "diagnostics";

    private const string FileNameHeader = "X-Vorcall-Filename";
    private const string TooLargeDetail = "reports must be 4 MiB or smaller";

    public static void Map(WebApplication app)
    {
        app.MapPost("/api/diagnostics", UploadAsync).RequireAuthorization().RequireRateLimiting(RateLimitPolicy);
    }

    // POST /api/diagnostics?kind=log|crash, the body being the raw UTF-8 text -> 204.
    private static async Task<IResult> UploadAsync(HttpContext context, string? kind, DiagnosticsStore store)
    {
        // The token validated, so a missing claim is this server's own bug.
        if (!BearerIdentity.TryGetUserId(context.User, out var userId))
        {
            return ProtobufBody.Fail(StatusCodes.Status401Unauthorized, "invalid bearer");
        }

        if (context.Request.ContentLength > DiagnosticsOptions.MaxFileBytes)
        {
            return ProtobufBody.Fail(StatusCodes.Status413PayloadTooLarge, TooLargeDetail);
        }

        // Kestrel's own cap would answer a lying Content-Length with its generic 413; one byte
        // past the limit lets the store see the overrun first and answer in protobuf.
        if (context.Features.Get<IHttpMaxRequestBodySizeFeature>() is { IsReadOnly: false } bodySize)
        {
            bodySize.MaxRequestBodySize = DiagnosticsOptions.MaxFileBytes + 1;
        }

        var result = await store.StoreAsync(
            userId,
            BearerIdentity.GetUsername(context.User),
            kind ?? string.Empty,
            context.Request.Headers[FileNameHeader].FirstOrDefault() ?? string.Empty,
            context.Request.Body,
            context.RequestAborted);

        return result switch
        {
            StoreResult.Stored => Results.NoContent(),
            StoreResult.TooLarge => ProtobufBody.Fail(StatusCodes.Status413PayloadTooLarge, TooLargeDetail),
            StoreResult.NotText => ProtobufBody.Fail(StatusCodes.Status400BadRequest, "not text"),
            StoreResult.InvalidName => ProtobufBody.Fail(StatusCodes.Status400BadRequest, "invalid file name"),
            StoreResult.InvalidKind => ProtobufBody.Fail(StatusCodes.Status400BadRequest, "invalid kind"),
            _ => ProtobufBody.Fail(StatusCodes.Status500InternalServerError, "storage failed"),
        };
    }
}
