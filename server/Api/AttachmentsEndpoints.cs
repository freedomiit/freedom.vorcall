using Microsoft.AspNetCore.Http.Features;
using Microsoft.Net.Http.Headers;
using Vorcall.Server.Attachments;
using Vorcall.Server.Auth;
using Vorcall.Server.Chat;
using Vorcall.Server.Permissions;

namespace Vorcall.Server.Api;

// Under /api so the pre-shared key middleware covers them, and behind a bearer on top. The
// upload streams straight into the store: nothing here holds an image in memory, and the only
// filesystem path in play is the one the store builds from the row id.
public static class AttachmentsEndpoints
{
    public const string RateLimitPolicy = "uploads";

    // The image endpoints answer an upload the same way this one does, so the bodies that say why
    // a picture was refused live here and are shared rather than spelled twice.
    internal const string UnsupportedTypeDetail = "unsupported image type";
    internal const string LengthRequiredDetail = "length required";
    internal const string TooLargeDetail = "images must be 8 MiB or smaller";
    internal const string NotAnImageDetail = "body is not the declared image type";
    internal const string TruncatedDetail = "body ended early";

    // Images are charged to the attachment quota, so a full store says so with one body.
    internal const string StorageFullDetail = "attachment storage is full";

    // One body for both misses — a row that is gone and a row whose file is gone look the same
    // from outside.
    private const string NotFoundDetail = "no such attachment";

    private const string FileNameHeader = "X-Vorcall-Filename";
    private const string ChannelField = "channel";

    // A static class cannot be a type argument, so the log category is named rather than taken
    // from ILogger<T>.
    private const string LogCategory = "Vorcall.Server.Api.AttachmentsEndpoints";

    public static void Map(WebApplication app)
    {
        app.MapPost("/api/attachments", UploadAsync).RequireAuthorization().RequireRateLimiting(RateLimitPolicy);
        app.MapGet("/api/attachments/{id:long}", GetAsync).RequireAuthorization();
    }

    // POST /api/attachments?channel=<id>, the body being the raw image bytes -> 201 Attachment.
    private static async Task<IResult> UploadAsync(
        HttpContext context,
        string? channel,
        AttachmentStore store,
        ConnectionRegistry registry,
        ILoggerFactory loggers)
    {
        // An upload has to name the channel it is meant for: that is what the link check later
        // compares a SendMessage against.
        if (!Validation.TryParseChannelId(channel, out var channelId))
        {
            return ProtobufBody.Fail(StatusCodes.Status400BadRequest, ChannelField);
        }

        // The token validated, so a missing claim is this server's own bug.
        if (!BearerIdentity.TryGetUserId(context.User, out var userId))
        {
            return ProtobufBody.Fail(StatusCodes.Status401Unauthorized, "invalid bearer");
        }

        // A hidden channel and a missing one answer alike, as everywhere else.
        if (registry.ChannelOf(channelId) is not { } info || !registry.CanView(userId, channelId))
        {
            return ProtobufBody.Fail(StatusCodes.Status403Forbidden, PermNames.Name(Perm.ViewChannel));
        }

        // A voice channel carries no messages, so nothing could ever link the upload.
        if (info.Kind == Data.ChannelKind.Voice)
        {
            return ProtobufBody.Fail(StatusCodes.Status400BadRequest, ChannelField);
        }

        if (!registry.Has(userId, channelId, Perm.AttachFiles))
        {
            return ProtobufBody.Fail(StatusCodes.Status403Forbidden, PermNames.Name(Perm.AttachFiles));
        }

        // Media type only: a charset or boundary parameter is none of this endpoint's business,
        // and media types are case-insensitive while the store's table is lower case.
        if (!MediaTypeHeaderValue.TryParse(context.Request.ContentType, out var parsedContentType)
            || parsedContentType.MediaType.Value is not { } declaredType
            || !AttachmentsOptions.TryExtension(declaredType.ToLowerInvariant(), out _))
        {
            return ProtobufBody.Fail(StatusCodes.Status415UnsupportedMediaType, UnsupportedTypeDetail);
        }

        var contentType = declaredType.ToLowerInvariant();

        // The declared length is what the quota is charged against, so an upload without one is
        // refused before it is read rather than trusted to stop on its own.
        if (context.Request.ContentLength is not { } declaredLength)
        {
            return ProtobufBody.Fail(StatusCodes.Status411LengthRequired, LengthRequiredDetail);
        }

        if (declaredLength > AttachmentsOptions.MaxFileBytes)
        {
            return ProtobufBody.Fail(StatusCodes.Status413PayloadTooLarge, TooLargeDetail);
        }

        // Kestrel's own 30 MB cap would answer a lying Content-Length with its generic 413; one
        // byte past the limit lets the store see the overrun first and answer in protobuf.
        if (context.Features.Get<IHttpMaxRequestBodySizeFeature>() is { IsReadOnly: false } bodySize)
        {
            bodySize.MaxRequestBodySize = AttachmentsOptions.MaxFileBytes + 1;
        }

        var outcome = await store.StoreAsync(
            userId,
            channelId,
            contentType,
            context.Request.Headers[FileNameHeader].FirstOrDefault(),
            declaredLength,
            context.Request.Body,
            context.RequestAborted);

        switch (outcome.Status)
        {
            case StoreOutcome.Kind.Stored when outcome.Attachment is { } attachment:
                loggers.CreateLogger(LogCategory).LogDebug(
                    "Attachment {AttachmentId} uploaded by {UserId} to {ChannelId} ({Size} bytes)",
                    attachment.Id,
                    userId,
                    channelId,
                    attachment.Size);
                return ProtobufBody.Proto(attachment, StatusCodes.Status201Created);
            case StoreOutcome.Kind.QuotaExceeded:
                return ProtobufBody.Fail(StatusCodes.Status507InsufficientStorage, StorageFullDetail);
            case StoreOutcome.Kind.TooLarge:
                return ProtobufBody.Fail(StatusCodes.Status413PayloadTooLarge, TooLargeDetail);
            case StoreOutcome.Kind.NotAnImage:
                return ProtobufBody.Fail(StatusCodes.Status400BadRequest, NotAnImageDetail);
            case StoreOutcome.Kind.StorageFailed:
                return ProtobufBody.Fail(StatusCodes.Status500InternalServerError, "attachment storage failed");
            case StoreOutcome.Kind.Truncated:
            default:
                return ProtobufBody.Fail(StatusCodes.Status400BadRequest, TruncatedDetail);
        }
    }

    // GET /api/attachments/{id} -> the bytes, Range supported.
    private static async Task<IResult> GetAsync(
        HttpContext context,
        long id,
        AttachmentStore store,
        ConnectionRegistry registry,
        ILoggerFactory loggers)
    {
        if (await store.FindAsync(id) is not { } row)
        {
            return ProtobufBody.Fail(StatusCodes.Status404NotFound, NotFoundDetail);
        }

        if (!BearerIdentity.TryGetUserId(context.User, out var userId))
        {
            return ProtobufBody.Fail(StatusCodes.Status401Unauthorized, "invalid bearer");
        }

        // Before a message names it, an upload belongs to whoever made it; after, it belongs to
        // the channel that message is in, which the uploader may have lost sight of since.
        if (row.MessageId is null)
        {
            if (row.UploaderId != userId)
            {
                return ProtobufBody.Fail(StatusCodes.Status403Forbidden, "not yours");
            }
        }
        else if (!registry.CanView(userId, row.ChannelId))
        {
            return ProtobufBody.Fail(StatusCodes.Status403Forbidden, PermNames.Name(Perm.ViewChannel));
        }

        var logger = loggers.CreateLogger(LogCategory);
        var path = store.PathFor(id, row.ContentType);
        if (!File.Exists(path))
        {
            logger.LogWarning("Attachment {AttachmentId} has a row but no file on disk", id);
            return ProtobufBody.Fail(StatusCodes.Status404NotFound, NotFoundDetail);
        }

        // The bytes under an id never change, so the client may keep them; private because the
        // view check above is what decides who gets them.
        context.Response.Headers.CacheControl = "private, max-age=31536000, immutable";

        logger.LogDebug(
            "Attachment {AttachmentId} served to {UserId} from {ChannelId} ({Size} bytes)",
            id,
            userId,
            row.ChannelId,
            row.Size);

        // Id and size are both numbers, so the tag always parses; a failure would cost
        // revalidation, never the download.
        var entityTag = EntityTagHeaderValue.TryParse($"\"{id}-{row.Size}\"", out var parsed) ? parsed : null;
        return Results.File(
            path,
            contentType: row.ContentType,
            fileDownloadName: null,
            lastModified: File.GetLastWriteTimeUtc(path),
            entityTag: entityTag,
            enableRangeProcessing: true);
    }
}
