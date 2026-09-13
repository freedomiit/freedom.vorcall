using Microsoft.AspNetCore.Http.Features;
using Microsoft.Net.Http.Headers;
using Vorcall.Server.Attachments;
using Vorcall.Server.Auth;
using Vorcall.Server.Chat;
using Vorcall.Server.Permissions;

namespace Vorcall.Server.Api;

// Avatars, banners, the server icon and role icons. Same door key, bearer, storage quota and
// upload rate limit as an attachment, but where an attachment takes any file up to 2 GiB this
// takes four image types, magic-checked, up to 8 MiB. What else differs is the purpose the upload
// declares, which is what decides the permission it needs, and that any bearer may read any image
// back.
public static class ImagesEndpoints
{
    private const string PurposeField = "purpose";
    private const string StorageFailedDetail = "image storage failed";

    // One body for both misses — a row that is gone and a row whose file is gone look the same
    // from outside.
    private const string NotFoundDetail = "no such image";

    // A static class cannot be a type argument, so the log category is named rather than taken
    // from ILogger<T>.
    private const string LogCategory = "Vorcall.Server.Api.ImagesEndpoints";

    public static void Map(WebApplication app)
    {
        app.MapPost("/api/images", UploadAsync)
            .RequireAuthorization()
            .RequireRateLimiting(AttachmentsEndpoints.RateLimitPolicy);
        app.MapGet("/api/images/{id:long}", GetAsync).RequireAuthorization();
    }

    // POST /api/images?purpose=avatar|banner|server_icon|role_icon, the body being the raw image
    // bytes -> 201 Image.
    private static async Task<IResult> UploadAsync(
        HttpContext context,
        string? purpose,
        ImageStore store,
        ConnectionRegistry registry,
        ILoggerFactory loggers)
    {
        if (ParsePurpose(purpose) is not { } declaredPurpose)
        {
            return ProtobufBody.Fail(StatusCodes.Status400BadRequest, PurposeField);
        }

        // The token validated, so a missing claim is this server's own bug.
        if (!BearerIdentity.TryGetUserId(context.User, out var userId))
        {
            return ProtobufBody.Fail(StatusCodes.Status401Unauthorized, "invalid bearer");
        }

        // Both bits are server-scoped, hence the null channel. An avatar or a banner needs nothing
        // beyond an account: the frame that later references one is what checks it belongs to the
        // caller.
        if (RequiredPermission(declaredPurpose) is { } required && !registry.Has(userId, null, required))
        {
            return ProtobufBody.Fail(StatusCodes.Status403Forbidden, PermNames.Name(required));
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

        if (declaredLength > AttachmentsOptions.ImageMaxFileBytes)
        {
            return ProtobufBody.Fail(StatusCodes.Status413PayloadTooLarge, AttachmentsEndpoints.TooLargeDetail);
        }

        // Kestrel's default body limit (30 MB) sits well above the 8 MiB image cap, so a lying
        // Content-Length would reach that generic 413 instead of ours; one byte past our own cap
        // lets the store see the overrun first and answer in protobuf.
        if (context.Features.Get<IHttpMaxRequestBodySizeFeature>() is { IsReadOnly: false } bodySize)
        {
            bodySize.MaxRequestBodySize = AttachmentsOptions.ImageMaxFileBytes + 1;
        }

        var outcome = await store.SaveAsync(
            declaredPurpose,
            userId,
            contentType,
            declaredLength,
            context.Request.Body,
            context.RequestAborted);

        switch (outcome.Status)
        {
            case ImageSaveOutcome.Kind.Saved when outcome.Image is { } image:
                loggers.CreateLogger(LogCategory).LogDebug(
                    "Image {ImageId} uploaded by {UserId} as {Purpose} ({Size} bytes)",
                    image.Id,
                    userId,
                    declaredPurpose,
                    image.Size);
                return ProtobufBody.Proto(
                    new Protocol.Image { Id = image.Id, ContentType = image.ContentType, Size = image.Size },
                    StatusCodes.Status201Created);
            case ImageSaveOutcome.Kind.QuotaExceeded:
                return ProtobufBody.Fail(StatusCodes.Status507InsufficientStorage, AttachmentsEndpoints.StorageFullDetail);
            case ImageSaveOutcome.Kind.TooLarge:
                return ProtobufBody.Fail(StatusCodes.Status413PayloadTooLarge, AttachmentsEndpoints.TooLargeDetail);
            case ImageSaveOutcome.Kind.BadMagic:
                return ProtobufBody.Fail(StatusCodes.Status400BadRequest, AttachmentsEndpoints.NotAnImageDetail);
            case ImageSaveOutcome.Kind.IoError:
                return ProtobufBody.Fail(StatusCodes.Status500InternalServerError, StorageFailedDetail);
            case ImageSaveOutcome.Kind.Truncated:
            default:
                return ProtobufBody.Fail(StatusCodes.Status400BadRequest, AttachmentsEndpoints.TruncatedDetail);
        }
    }

    // GET /api/images/{id} -> the bytes, Range supported. Any bearer may read any image: an id is
    // all a client has, and every image it could name is already shown to every member.
    private static async Task<IResult> GetAsync(HttpContext context, long id, ImageStore store, ILoggerFactory loggers)
    {
        if (await store.GetAsync(id, context.RequestAborted) is not { } found)
        {
            return ProtobufBody.Fail(StatusCodes.Status404NotFound, NotFoundDetail);
        }

        if (!File.Exists(found.Path))
        {
            loggers.CreateLogger(LogCategory).LogWarning("Image {ImageId} has a row but no file on disk", id);
            return ProtobufBody.Fail(StatusCodes.Status404NotFound, NotFoundDetail);
        }

        // The bytes under an id never change — a new picture is a new row — so a client may keep
        // them for good; private because a bearer is still what gets them.
        context.Response.Headers.CacheControl = "private, max-age=31536000, immutable";

        return Results.File(
            found.Path,
            contentType: found.Image.ContentType,
            enableRangeProcessing: true,
            entityTag: new EntityTagHeaderValue($"\"{id}\""));
    }

    private static Data.ImagePurpose? ParsePurpose(string? raw) => raw switch
    {
        "avatar" => Data.ImagePurpose.Avatar,
        "banner" => Data.ImagePurpose.Banner,
        "server_icon" => Data.ImagePurpose.ServerIcon,
        "role_icon" => Data.ImagePurpose.RoleIcon,
        _ => null,
    };

    private static Perm? RequiredPermission(Data.ImagePurpose purpose) => purpose switch
    {
        Data.ImagePurpose.ServerIcon => Perm.ManageServer,
        Data.ImagePurpose.RoleIcon => Perm.ManageRoles,
        _ => null,
    };
}
