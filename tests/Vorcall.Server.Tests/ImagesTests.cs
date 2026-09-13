using System.Net;
using Vorcall.Server.Attachments;
using Vorcall.Server.Permissions;
using Vorcall.Server.Protocol;
using Vorcall.Server.Tests.Infrastructure;
using Xunit;

namespace Vorcall.Server.Tests;

// The other half of the type split: an attachment loosened to any file of any type, and the image
// store did not. Avatars, banners, the server icon and role icons still take four types, each
// magic-checked, at 8 MiB — and nothing else proves that first half.
[Collection(ServerCollection.Name)]
public sealed class ImagesTests(ServerFixture fixture)
{
    private VorcallFactory Server => fixture.Server;

    [Fact]
    public async Task An_image_is_stored_under_a_purpose_and_read_back_by_any_bearer_with_ETag_and_Range()
    {
        await using var party = await Party.ConnectAsync(Server, "xena", "yara");
        var (xena, yara) = (party.Account(0), party.Account(1));
        var pixel = Png.Pixel();

        var uploaded = await Uploads.UploadImageAsync(Server, xena.Access, "avatar", pixel);
        Assert.Equal(HttpStatusCode.Created, uploaded.Status);
        var image = uploaded.As(Image.Parser);
        Assert.True(image.Id > 0);
        Assert.Equal("image/png", image.ContentType);
        Assert.Equal((long)pixel.Length, image.Size);

        // An id is all a client has and every image it could name is already shown to every
        // member, so any bearer may read any image.
        var served = await Uploads.DownloadImageAsync(Server, yara.Access, image.Id);
        Assert.Equal(HttpStatusCode.OK, served.Status);
        Assert.Equal(pixel, served.Body);
        Assert.Equal("image/png", served.Header("Content-Type"));

        // An image's bytes never change — a new picture is a new row — so the tag is the id
        // alone, unlike an attachment's "<id>-<size>".
        Assert.Equal($"\"{image.Id}\"", served.Header("ETag"));
        Assert.Equal(["immutable", "max-age=31536000", "private"], served.Parts("Cache-Control"));

        var head = await Uploads.DownloadImageAsync(Server, yara.Access, image.Id, range: "bytes=0-7");
        Assert.Equal(HttpStatusCode.PartialContent, head.Status);
        Assert.Equal(pixel[..8], head.Body);

        var missing = await Uploads.DownloadImageAsync(Server, yara.Access, image.Id + 1_000_000);
        Assert.Equal(HttpStatusCode.NotFound, missing.Status);
        Assert.Equal("no such image", missing.Detail);
    }

    [Fact]
    public async Task The_image_store_takes_four_types_and_holds_every_one_of_them_to_its_magic_number()
    {
        await using var party = await Party.ConnectAsync(Server, "zora");
        var zora = party.Account(0);
        var pixel = Png.Pixel();

        // A type that is not on the table is refused for being the wrong type, before any byte of
        // the body is looked at. An attachment would have taken all three of these.
        foreach (var type in new[] { "image/bmp", "text/plain", StreamApi.Octets })
        {
            var refused = await Uploads.UploadImageAsync(Server, zora.Access, "avatar", pixel, type);
            Assert.True(
                refused.Status == HttpStatusCode.UnsupportedMediaType,
                $"{type}: {(int)refused.Status}");
            Assert.Equal("unsupported image type", refused.Detail);
        }

        // The other three of the four are on the table, so a PNG body under one of their names
        // gets past the type check and is caught by the magic check instead: a different refusal,
        // which is what says the table and the sniffer are two separate rules.
        foreach (var type in new[] { "image/jpeg", "image/gif", "image/webp" })
        {
            var refused = await Uploads.UploadImageAsync(Server, zora.Access, "avatar", pixel, type);
            Assert.True(refused.Status == HttpStatusCode.BadRequest, $"{type}: {(int)refused.Status}");
            Assert.Equal("body is not the declared image type", refused.Detail);
        }
    }

    [Fact]
    public async Task A_renamed_file_declared_as_a_picture_is_refused_as_not_being_one()
    {
        await using var party = await Party.ConnectAsync(Server, "adele");
        var adele = party.Account(0);

        // The whole point of the magic check: a client renaming a program to .png and declaring
        // image/png stores nothing.
        var executable = Blob.Text("MZ\0\0\0\0\0\0\0this is a PE header, not a picture");
        var refused = await Uploads.UploadImageAsync(Server, adele.Access, "avatar", executable);
        Assert.Equal(HttpStatusCode.BadRequest, refused.Status);
        Assert.Equal("body is not the declared image type", refused.Detail);

        // And a body too short to carry a magic number at all is refused the same way rather than
        // slipping through unchecked.
        var stub = await Uploads.UploadImageAsync(Server, adele.Access, "avatar", Blob.Text("PNG"));
        Assert.Equal(HttpStatusCode.BadRequest, stub.Status);
        Assert.Equal("body is not the declared image type", stub.Detail);

        // The same body as an attachment is a file like any other: this is the split.
        var asAttachment = await Uploads.UploadAsync(Server, adele.Access, party.GeneralId, executable, StreamApi.Octets, "setup.exe");
        Assert.Equal(HttpStatusCode.Created, asAttachment.Status);
    }

    [Fact]
    public async Task An_image_past_eight_mebibytes_is_refused_while_an_attachment_that_size_is_not()
    {
        await using var party = await Party.ConnectAsync(Server, "brisa");
        var brisa = party.Account(0);
        var pixel = Png.Pixel();
        var nineMebibytes = (long)AttachmentsOptions.ImageMaxFileBytes + (1 << 20);

        // Refused from the declared length alone, so the test needs no nine megabytes of its own.
        var refused = await Uploads.UploadImageAsync(Server, brisa.Access, "avatar", pixel, declaredLength: nineMebibytes);
        Assert.Equal(HttpStatusCode.RequestEntityTooLarge, refused.Status);
        Assert.Equal("images must be 8 MiB or smaller", refused.Detail);

        // The attachment cap is 2 GiB, so the same length is nowhere near it and the upload is
        // refused for its body ending early instead.
        var asAttachment = await Uploads.UploadAsync(
            Server,
            brisa.Access,
            party.GeneralId,
            pixel,
            StreamApi.Octets,
            declaredLength: nineMebibytes);
        Assert.Equal(HttpStatusCode.BadRequest, asAttachment.Status);
        Assert.Equal("body ended early", asAttachment.Detail);
    }

    [Fact]
    public async Task Only_the_two_icon_purposes_need_a_permission_and_an_unknown_purpose_names_none()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture, "cleo");
        var (owner, cleo) = (party.Account(0), party.Account(1));
        var pixel = Png.Pixel();

        foreach (var purpose in new[] { string.Empty, "icon", "AVATAR" })
        {
            var refused = await Uploads.UploadImageAsync(Server, cleo.Access, purpose, pixel);
            Assert.True(refused.Status == HttpStatusCode.BadRequest, $"'{purpose}': {(int)refused.Status}");
            Assert.Equal("purpose", refused.Detail);
        }

        // An avatar or a banner needs nothing beyond an account; the frame that later references
        // one is what checks it belongs to the caller.
        foreach (var purpose in new[] { "avatar", "banner" })
        {
            var mine = await Uploads.UploadImageAsync(Server, cleo.Access, purpose, pixel);
            Assert.True(mine.Status == HttpStatusCode.Created, $"{purpose}: {(int)mine.Status}");
        }

        // The two server-scoped ones do, and each names the bit it wanted.
        var icon = await Uploads.UploadImageAsync(Server, cleo.Access, "server_icon", pixel);
        Assert.Equal(HttpStatusCode.Forbidden, icon.Status);
        Assert.Equal(PermNames.Name(Perm.ManageServer), icon.Detail);

        var roleIcon = await Uploads.UploadImageAsync(Server, cleo.Access, "role_icon", pixel);
        Assert.Equal(HttpStatusCode.Forbidden, roleIcon.Status);
        Assert.Equal(PermNames.Name(Perm.ManageRoles), roleIcon.Detail);

        // The owner bypasses every check, so the same two uploads land.
        foreach (var purpose in new[] { "server_icon", "role_icon" })
        {
            var allowed = await Uploads.UploadImageAsync(Server, owner.Access, purpose, pixel);
            Assert.True(allowed.Status == HttpStatusCode.Created, $"{purpose}: {(int)allowed.Status}");
        }
    }
}
