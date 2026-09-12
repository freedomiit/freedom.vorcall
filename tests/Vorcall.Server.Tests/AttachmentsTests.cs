using System.Net;
using Microsoft.Extensions.DependencyInjection;
using Vorcall.Server.Attachments;
using Vorcall.Server.Protocol;
using Vorcall.Server.Tests.Infrastructure;
using Xunit;
using Kind = Vorcall.Server.Protocol.ServerFrame.PayloadOneofCase;

namespace Vorcall.Server.Tests;

[Collection(ServerCollection.Name)]
public sealed class AttachmentsTests(ServerFixture fixture)
{
    private VorcallFactory Server => fixture.Server;

    [Fact]
    public async Task Upload_enforces_the_type_the_magic_bytes_the_size_and_the_membership()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "carol");
        var (alice, carol) = (party.Account(0), party.Account(1));
        var (_, roomId) = await party.CreateRoomAsync();
        var pixel = Png.Pixel();

        var stored = await Uploads.UploadAsync(Server, alice.Access, Session.General, pixel, fileName: "pixel.png");
        Assert.Equal(HttpStatusCode.Created, stored.Status);
        var attachment = stored.As(Attachment.Parser);
        Assert.True(attachment.Id > 0);
        Assert.Equal("pixel.png", attachment.FileName);
        Assert.Equal("image/png", attachment.ContentType);
        Assert.Equal((long)pixel.Length, attachment.Size);

        var bmp = await Uploads.UploadAsync(Server, alice.Access, Session.General, pixel, contentType: "image/bmp");
        Assert.Equal(HttpStatusCode.UnsupportedMediaType, bmp.Status);
        Assert.Equal("unsupported image type", bmp.Detail);

        var notAnImage = await Uploads.UploadAsync(Server, alice.Access, Session.General, "MZ not an image at all"u8.ToArray());
        Assert.Equal(HttpStatusCode.BadRequest, notAnImage.Status);
        Assert.Equal("body is not the declared image type", notAnImage.Detail);

        // The header alone: 9 MiB is above the cap, so the length is refused before any body is read.
        var tooLarge = await Uploads.UploadAsync(Server, alice.Access, Session.General, pixel, declaredLength: 9L * 1024 * 1024);
        Assert.Equal(HttpStatusCode.RequestEntityTooLarge, tooLarge.Status);
        Assert.Equal("images must be 8 MiB or smaller", tooLarge.Detail);

        // Carol never joined the room alice created.
        var outsider = await Uploads.UploadAsync(Server, carol.Access, roomId, pixel);
        Assert.Equal(HttpStatusCode.Forbidden, outsider.Status);
        Assert.Equal("not a member", outsider.Detail);
    }

    [Fact]
    public async Task An_unlinked_upload_belongs_to_its_uploader_alone()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob");
        var (alice, bob) = (party.Account(0), party.Account(1));
        var pixel = Png.Pixel();
        var attachment = await UploadPixelAsync(alice, Session.General, pixel);

        var stranger = await Uploads.DownloadAsync(Server, bob.Access, attachment.Id);
        Assert.Equal(HttpStatusCode.Forbidden, stranger.Status);

        var owner = await Uploads.DownloadAsync(Server, alice.Access, attachment.Id);
        Assert.Equal(HttpStatusCode.OK, owner.Status);
        Assert.Equal(pixel, owner.Body);
    }

    [Fact]
    public async Task A_linked_upload_is_readable_by_the_room_with_ETag_and_Range()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob", "carol");
        var (alice, a1, bob) = (party.Account(0), party.Client(0), party.Account(1));
        var pixel = Png.Pixel();
        var attachment = await UploadPixelAsync(alice, Session.General, pixel);

        var caption = $"look at this {Names.Token()}";
        await a1.SendAsync(Frames.Send(caption, Session.General, 0, attachment.Id));
        foreach (var frame in await party.ExpectEverywhereAsync(Kind.Message))
        {
            var message = frame.Message;
            Assert.Equal(caption, message.Text);
            Assert.Equal(new[] { attachment.Id }, message.Attachments.Select(item => item.Id).ToArray());
            Assert.Equal("pixel.png", message.Attachments[0].FileName);
            Assert.Equal((long)pixel.Length, message.Attachments[0].Size);
        }

        var whole = await Uploads.DownloadAsync(Server, bob.Access, attachment.Id);
        Assert.Equal(HttpStatusCode.OK, whole.Status);
        Assert.Equal(pixel, whole.Body);
        Assert.Equal($"\"{attachment.Id}-{pixel.Length}\"", whole.Header("ETag"));

        var head = await Uploads.DownloadAsync(Server, bob.Access, attachment.Id, range: "bytes=0-3");
        Assert.Equal(HttpStatusCode.PartialContent, head.Status);
        Assert.Equal(pixel[..4], head.Body);
    }

    [Fact]
    public async Task A_DM_image_is_invisible_outside_the_DM()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob", "carol");
        var (alice, a1, b1) = (party.Account(0), party.Client(0), party.Client(1));
        var (bob, carol, c1) = (party.Account(1), party.Account(2), party.Client(2));
        var dmId = await party.OpenDmAsync(0, 1);

        var attachment = await UploadPixelAsync(alice, dmId, Png.Pixel(), "private.png");
        await a1.SendAsync(Frames.Send(string.Empty, dmId, 0, attachment.Id));
        foreach (var client in new[] { a1, b1 })
        {
            var message = (await client.ExpectAsync(Kind.Message)).Message;
            Assert.Equal(string.Empty, message.Text);
            Assert.Equal(dmId, message.RoomId);
            Assert.Equal(new[] { attachment.Id }, message.Attachments.Select(item => item.Id).ToArray());
        }

        var outsider = await Uploads.DownloadAsync(Server, carol.Access, attachment.Id);
        Assert.Equal(HttpStatusCode.Forbidden, outsider.Status);
        var member = await Uploads.DownloadAsync(Server, bob.Access, attachment.Id);
        Assert.Equal(HttpStatusCode.OK, member.Status);
        await c1.QuietAsync();
    }

    [Fact]
    public async Task Reuse_or_too_many_attachments_answer_INVALID_ATTACHMENT_and_deleting_the_message_removes_the_file()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob", "carol");
        var (alice, a1) = (party.Account(0), party.Client(0));
        var attachment = await UploadPixelAsync(alice, Session.General, Png.Pixel());

        var caption = $"linked {Names.Token()}";
        await a1.SendAsync(Frames.Send(caption, Session.General, 0, attachment.Id));
        var imageMessageId = await party.ExpectMessageEverywhereAsync(caption);

        await a1.SendAsync(Frames.Send("again", Session.General, 0, attachment.Id));
        await a1.ExpectErrorAsync(ErrorCode.InvalidAttachment, fatal: false);

        var five = Enumerable.Range(1, 5).Select(offset => attachment.Id + offset).ToArray();
        await a1.SendAsync(Frames.Send("five", Session.General, 0, five));
        await a1.ExpectErrorAsync(ErrorCode.InvalidAttachment, fatal: false);

        await a1.SendAsync(Frames.Delete(imageMessageId));
        foreach (var frame in await party.ExpectEverywhereAsync(Kind.MessageDeleted))
        {
            Assert.Equal(imageMessageId, frame.MessageDeleted.Id);
        }

        var gone = await Uploads.DownloadAsync(Server, alice.Access, attachment.Id);
        Assert.Equal(HttpStatusCode.NotFound, gone.Status);
        var store = Server.Services.GetRequiredService<AttachmentStore>();
        Assert.False(File.Exists(store.PathFor(attachment.Id, attachment.ContentType)));
    }

    private async Task<Attachment> UploadPixelAsync(Account uploader, string roomId, byte[] pixel, string fileName = "pixel.png")
    {
        var response = await Uploads.UploadAsync(Server, uploader.Access, roomId, pixel, fileName: fileName);
        Assert.True(response.Status == HttpStatusCode.Created, $"upload to {roomId}: {(int)response.Status}");
        return response.As(Attachment.Parser);
    }
}
