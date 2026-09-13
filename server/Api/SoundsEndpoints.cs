using Microsoft.AspNetCore.Http.Features;
using Microsoft.Net.Http.Headers;
using Vorcall.Server.Attachments;
using Vorcall.Server.Auth;
using Vorcall.Server.Chat;
using Vorcall.Server.Permissions;

namespace Vorcall.Server.Api;

// The soundpad library's bytes. The same door key, bearer, storage quota and upload rate limit as
// an attachment, and the shape of the image endpoints: a raw body streamed straight into the
// store, and any bearer may read anything back. What differs is that only MANAGE_SOUNDS may add a
// clip, and that the body must be the VORCSND1 container of PROTOCOL.md § Sounds — which the
// store walks as it streams and never decodes.
public static class SoundsEndpoints
{
    private const string NameField = "name";
    private const string StorageFailedDetail = "sound storage failed";

    internal const string UnsupportedTypeDetail = "sounds must be " + AttachmentsOptions.SoundMediaType;
    internal const string TooLargeDetail = "sounds must be 16 MiB or smaller";
    internal const string MalformedDetail = "body is not a VORCSND1 sound container";

    // One body for both misses — a row that is gone, a row whose bytes never arrived and a row
    // whose file is gone look the same from outside.
    private const string NotFoundDetail = "no such sound";

    // A static class cannot be a type argument, so the log category is named rather than taken
    // from ILogger<T>.
    private const string LogCategory = "Vorcall.Server.Api.SoundsEndpoints";

    public static void Map(WebApplication app)
    {
        app.MapPost("/api/sounds", UploadAsync)
            .RequireAuthorization()
            .RequireRateLimiting(AttachmentsEndpoints.RateLimitPolicy);
        app.MapGet("/api/sounds/{id:long}", GetAsync).RequireAuthorization();
    }

    // POST /api/sounds?name=<name>, the body being the raw container bytes -> 201 Sound.
    private static async Task<IResult> UploadAsync(
        HttpContext context,
        string? name,
        SoundStore store,
        ConnectionRegistry registry,
        ILoggerFactory loggers)
    {
        // The token validated, so a missing claim is this server's own bug.
        if (!BearerIdentity.TryGetUserId(context.User, out var userId))
        {
            return ProtobufBody.Fail(StatusCodes.Status401Unauthorized, "invalid bearer");
        }

        // Server-scoped, hence the null channel: no override can grant the right to add a clip.
        if (!registry.Has(userId, null, Perm.ManageSounds))
        {
            return ProtobufBody.Fail(StatusCodes.Status403Forbidden, PermNames.Name(Perm.ManageSounds));
        }

        // Media type only: a charset parameter is none of this endpoint's business, and media
        // types are case-insensitive while the constant is lower case.
        if (!MediaTypeHeaderValue.TryParse(context.Request.ContentType, out var parsedContentType)
            || parsedContentType.MediaType.Value is not { } declaredType
            || !string.Equals(declaredType, AttachmentsOptions.SoundMediaType, StringComparison.OrdinalIgnoreCase))
        {
            return ProtobufBody.Fail(StatusCodes.Status415UnsupportedMediaType, UnsupportedTypeDetail);
        }

        // The declared length is what the quota is charged against, so an upload without one is
        // refused before it is read rather than trusted to stop on its own.
        if (context.Request.ContentLength is not { } declaredLength)
        {
            return ProtobufBody.Fail(StatusCodes.Status411LengthRequired, AttachmentsEndpoints.LengthRequiredDetail);
        }

        if (declaredLength > AttachmentsOptions.SoundMaxFileBytes)
        {
            return ProtobufBody.Fail(StatusCodes.Status413PayloadTooLarge, TooLargeDetail);
        }

        // Kestrel's default body limit (30 MB) sits well above the 16 MiB clip cap, so a lying
        // Content-Length would reach that generic 413 instead of ours; one byte past our own cap
        // lets the store see the overrun first and answer in protobuf.
        if (context.Features.Get<IHttpMaxRequestBodySizeFeature>() is { IsReadOnly: false } bodySize)
        {
            bodySize.MaxRequestBodySize = AttachmentsOptions.SoundMaxFileBytes + 1;
        }

        var outcome = await store.SaveAsync(
            name ?? string.Empty,
            userId,
            declaredLength,
            context.Request.Body,
            context.RequestAborted);

        switch (outcome.Status)
        {
            case SoundSaveOutcome.Kind.Saved when outcome.Sound is { } sound:
                // The library is mirrored and broadcast only once the bytes are down, so nobody is
                // ever offered a clip this server cannot serve.
                registry.CreateSound(sound);
                loggers.CreateLogger(LogCategory).LogDebug(
                    "Sound {SoundId} uploaded by {UserId} ({Size} bytes, {DurationMs} ms)",
                    sound.Id,
                    userId,
                    sound.Size,
                    sound.DurationMs);
                return ProtobufBody.Proto(
                    new Protocol.Sound
                    {
                        Id = sound.Id,
                        Name = sound.Name,
                        UploaderId = sound.UploaderId ?? 0,
                        DurationMs = (uint)sound.DurationMs,
                        Size = sound.Size,
                    },
                    StatusCodes.Status201Created);
            case SoundSaveOutcome.Kind.QuotaExceeded:
                return ProtobufBody.Fail(StatusCodes.Status507InsufficientStorage, AttachmentsEndpoints.StorageFullDetail);
            case SoundSaveOutcome.Kind.TooLarge:
                return ProtobufBody.Fail(StatusCodes.Status413PayloadTooLarge, TooLargeDetail);
            case SoundSaveOutcome.Kind.Malformed:
                return ProtobufBody.Fail(StatusCodes.Status400BadRequest, MalformedDetail);
            case SoundSaveOutcome.Kind.IoError:
                return ProtobufBody.Fail(StatusCodes.Status500InternalServerError, StorageFailedDetail);
            case SoundSaveOutcome.Kind.Truncated:
            default:
                return ProtobufBody.Fail(StatusCodes.Status400BadRequest, AttachmentsEndpoints.TruncatedDetail);
        }
    }

    // GET /api/sounds/{id} -> the bytes, Range supported. Any bearer may read any clip: the whole
    // library is already offered to every member in ServerSnapshot.
    private static async Task<IResult> GetAsync(HttpContext context, long id, SoundStore store, ILoggerFactory loggers)
    {
        if (await store.GetAsync(id, context.RequestAborted) is not { } found)
        {
            return ProtobufBody.Fail(StatusCodes.Status404NotFound, NotFoundDetail);
        }

        if (!File.Exists(found.Path))
        {
            loggers.CreateLogger(LogCategory).LogWarning("Sound {SoundId} has a row but no file on disk", id);
            return ProtobufBody.Fail(StatusCodes.Status404NotFound, NotFoundDetail);
        }

        // The bytes under an id never change — a re-upload is a new row — so a client may keep
        // them for good; private because a bearer is still what gets them.
        context.Response.Headers.CacheControl = "private, max-age=31536000, immutable";

        return Results.File(
            found.Path,
            contentType: found.Sound.ContentType,
            enableRangeProcessing: true,
            entityTag: new EntityTagHeaderValue($"\"{id}\""));
    }
}
