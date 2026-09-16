using Microsoft.AspNetCore.Http.Features;
using Microsoft.Net.Http.Headers;
using Vorcall.Server.Attachments;
using Vorcall.Server.Auth;
using Vorcall.Server.Chat;
using Vorcall.Server.Permissions;

namespace Vorcall.Server.Api;

// The sticker library's bytes. The same door key, bearer, storage quota and upload rate limit as
// an attachment, and the soundpad's shape: a raw body streamed straight into the store, only
// MANAGE_STICKERS may add one, and any bearer may read anything back. The body is held to an
// image's rules — four types, magic-checked — at 1 MiB.
public static class StickersEndpoints
{
    private const string StorageFailedDetail = "sticker storage failed";

    // The same spelling UpdateSticker answers ERROR_CODE_INVALID_ARGUMENT with, so a client can
    // treat a name the grammar refuses alike over both paths.
    internal const string NameField = "name";

    internal const string TooLargeDetail = "stickers must be 1 MiB or smaller";
    internal const string LimitReachedDetail = "the sticker library is full";

    // One body for both misses — a row that is gone, a row whose bytes never arrived and a row
    // whose file is gone look the same from outside.
    private const string NotFoundDetail = "no such sticker";

    // A static class cannot be a type argument, so the log category is named rather than taken
    // from ILogger<T>.
    private const string LogCategory = "Vorcall.Server.Api.StickersEndpoints";

    public static void Map(WebApplication app)
    {
        app.MapPost("/api/stickers", UploadAsync)
            .RequireAuthorization()
            .RequireRateLimiting(AttachmentsEndpoints.RateLimitPolicy);
        app.MapGet("/api/stickers/{id:long}", GetAsync).RequireAuthorization();
    }

    // POST /api/stickers?name=<name>, the body being the raw image bytes -> 201 Sticker.
    private static async Task<IResult> UploadAsync(
        HttpContext context,
        string? name,
        StickerStore store,
        ConnectionRegistry registry,
        ILoggerFactory loggers)
    {
        // The token validated, so a missing claim is this server's own bug.
        if (!BearerIdentity.TryGetUserId(context.User, out var userId))
        {
            return ProtobufBody.Fail(StatusCodes.Status401Unauthorized, "invalid bearer");
        }

        // Server-scoped, hence the null channel: no override can grant the right to add a sticker.
        if (!registry.Has(userId, null, Perm.ManageStickers))
        {
            return ProtobufBody.Fail(StatusCodes.Status403Forbidden, PermNames.Name(Perm.ManageStickers));
        }

        // Media type only: a charset parameter is none of this endpoint's business, and media types
        // are case-insensitive while the store's table is lower case.
        if (!MediaTypeHeaderValue.TryParse(context.Request.ContentType, out var parsedContentType)
            || parsedContentType.MediaType.Value is not { } declaredType
            || !AttachmentsOptions.TryImageExtension(declaredType.ToLowerInvariant(), out _))
        {
            return ProtobufBody.Fail(StatusCodes.Status415UnsupportedMediaType, AttachmentsEndpoints.UnsupportedTypeDetail);
        }

        var contentType = declaredType.ToLowerInvariant();

        // The declared length is what the quota is charged against, so an upload without one is
        // refused before it is read rather than trusted to stop on its own.
        if (context.Request.ContentLength is not { } declaredLength)
        {
            return ProtobufBody.Fail(StatusCodes.Status411LengthRequired, AttachmentsEndpoints.LengthRequiredDetail);
        }

        if (declaredLength > AttachmentsOptions.StickerMaxFileBytes)
        {
            return ProtobufBody.Fail(StatusCodes.Status413PayloadTooLarge, TooLargeDetail);
        }

        // Kestrel's default body limit (30 MB) sits far above the 1 MiB sticker cap, so a lying
        // Content-Length would reach that generic 413 instead of ours; one byte past our own cap
        // lets the store see the overrun first and answer in protobuf.
        if (context.Features.Get<IHttpMaxRequestBodySizeFeature>() is { IsReadOnly: false } bodySize)
        {
            bodySize.MaxRequestBodySize = AttachmentsOptions.StickerMaxFileBytes + 1;
        }

        var outcome = await store.SaveAsync(
            name ?? string.Empty,
            userId,
            contentType,
            declaredLength,
            context.Request.Body,
            context.RequestAborted);

        switch (outcome.Status)
        {
            case StickerSaveOutcome.Kind.Saved when outcome.Sticker is { } sticker:
                // The library is mirrored and broadcast only once the bytes are down, so nobody is
                // ever offered a sticker this server cannot serve.
                registry.CreateSticker(sticker);
                loggers.CreateLogger(LogCategory).LogDebug(
                    "Sticker {StickerId} uploaded by {UserId} ({Size} bytes)",
                    sticker.Id,
                    userId,
                    sticker.Size);
                return ProtobufBody.Proto(
                    new Protocol.Sticker
                    {
                        Id = sticker.Id,
                        Name = sticker.Name,
                        UploaderId = sticker.UploaderId ?? 0,
                        ContentType = sticker.ContentType,
                        Size = sticker.Size,
                    },
                    StatusCodes.Status201Created);
            case StickerSaveOutcome.Kind.QuotaExceeded:
                return ProtobufBody.Fail(StatusCodes.Status507InsufficientStorage, AttachmentsEndpoints.StorageFullDetail);
            case StickerSaveOutcome.Kind.LimitReached:
                return ProtobufBody.Fail(StatusCodes.Status409Conflict, LimitReachedDetail);
            case StickerSaveOutcome.Kind.TooLarge:
                return ProtobufBody.Fail(StatusCodes.Status413PayloadTooLarge, TooLargeDetail);
            case StickerSaveOutcome.Kind.BadMagic:
                return ProtobufBody.Fail(StatusCodes.Status400BadRequest, AttachmentsEndpoints.NotAnImageDetail);
            case StickerSaveOutcome.Kind.InvalidName:
                return ProtobufBody.Fail(StatusCodes.Status400BadRequest, NameField);
            case StickerSaveOutcome.Kind.IoError:
                return ProtobufBody.Fail(StatusCodes.Status500InternalServerError, StorageFailedDetail);
            case StickerSaveOutcome.Kind.Truncated:
            default:
                return ProtobufBody.Fail(StatusCodes.Status400BadRequest, AttachmentsEndpoints.TruncatedDetail);
        }
    }

    // GET /api/stickers/{id} -> the bytes, Range supported. Any bearer may read any sticker: the
    // whole library is already offered to every member in ServerSnapshot.
    private static async Task<IResult> GetAsync(HttpContext context, long id, StickerStore store, ILoggerFactory loggers)
    {
        if (await store.GetAsync(id, context.RequestAborted) is not { } found)
        {
            return ProtobufBody.Fail(StatusCodes.Status404NotFound, NotFoundDetail);
        }

        if (!File.Exists(found.Path))
        {
            loggers.CreateLogger(LogCategory).LogWarning("Sticker {StickerId} has a row but no file on disk", id);
            return ProtobufBody.Fail(StatusCodes.Status404NotFound, NotFoundDetail);
        }

        // The bytes under an id never change — a re-upload is a new row — so a client may keep
        // them for good; private because a bearer is still what gets them.
        context.Response.Headers.CacheControl = "private, max-age=31536000, immutable";

        return Results.File(
            found.Path,
            contentType: found.Sticker.ContentType,
            enableRangeProcessing: true,
            entityTag: new EntityTagHeaderValue($"\"{id}\""));
    }
}
