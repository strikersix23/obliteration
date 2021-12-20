#include "Emulator/Graphics/VideoOutBuffer.h"

#include "Kyty/Core/DbgAssert.h"

#include "Emulator/Graphics/GraphicContext.h"
#include "Emulator/Graphics/GraphicsRender.h"
#include "Emulator/Graphics/Tile.h"
#include "Emulator/Graphics/Utils.h"
#include "Emulator/Profiler.h"

#include <vulkan/vulkan_core.h>

#ifdef KYTY_EMU_ENABLED

namespace Kyty::Libs::Graphics {

void* VideoOutBufferObject::Create(GraphicContext* ctx, const uint64_t* vaddr, const uint64_t* size, int vaddr_num, VulkanMemory* mem) const
{
	KYTY_PROFILER_BLOCK("VideoOutBufferObject::Create");

	EXIT_IF(vaddr_num != 1 || size == nullptr || vaddr == nullptr);
	EXIT_IF(mem == nullptr);
	EXIT_IF(ctx == nullptr);

	auto pixel_format = params[PARAM_FORMAT];
	auto width        = params[PARAM_WIDTH];
	auto height       = params[PARAM_HEIGHT];

	EXIT_NOT_IMPLEMENTED(pixel_format != 0x80000000);
	EXIT_NOT_IMPLEMENTED(width == 0);
	EXIT_NOT_IMPLEMENTED(height == 0);

	auto* vk_obj = new VideoOutVulkanImage;

	vk_obj->extent.width  = width;
	vk_obj->extent.height = height;
	vk_obj->format        = VK_FORMAT_B8G8R8A8_SRGB; // VK_FORMAT_R8G8B8A8_SRGB;
	vk_obj->image         = nullptr;
	vk_obj->image_view    = nullptr;

	VkImageCreateInfo image_info {};
	image_info.sType         = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO;
	image_info.pNext         = nullptr;
	image_info.flags         = 0;
	image_info.imageType     = VK_IMAGE_TYPE_2D;
	image_info.extent.width  = vk_obj->extent.width;
	image_info.extent.height = vk_obj->extent.height;
	image_info.extent.depth  = 1;
	image_info.mipLevels     = 1;
	image_info.arrayLayers   = 1;
	image_info.format        = vk_obj->format;
	image_info.tiling        = VK_IMAGE_TILING_OPTIMAL;
	image_info.initialLayout = VK_IMAGE_LAYOUT_UNDEFINED;
	image_info.usage         = static_cast<VkImageUsageFlags>(VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT | VK_IMAGE_USAGE_TRANSFER_SRC_BIT) |
	                   VK_IMAGE_USAGE_TRANSFER_DST_BIT;
	image_info.sharingMode = VK_SHARING_MODE_EXCLUSIVE;
	image_info.samples     = VK_SAMPLE_COUNT_1_BIT;

	vkCreateImage(ctx->device, &image_info, nullptr, &vk_obj->image);

	EXIT_NOT_IMPLEMENTED(vk_obj->image == nullptr);

	vkGetImageMemoryRequirements(ctx->device, vk_obj->image, &mem->requirements);

	mem->property = VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT;

	bool allocated = VulkanAllocate(ctx, mem);

	EXIT_NOT_IMPLEMENTED(!allocated);

	// vkBindImageMemory(ctx->device, vk_obj->image, mem->memory, mem->offset);
	VulkanBindImageMemory(ctx, vk_obj, mem);

	vk_obj->memory = *mem;

	EXIT_NOT_IMPLEMENTED(mem->requirements.size > *size);

	GetUpdateFunc()(ctx, params, vk_obj, vaddr, size, vaddr_num);

	VkImageViewCreateInfo create_info {};
	create_info.sType                           = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO;
	create_info.pNext                           = nullptr;
	create_info.flags                           = 0;
	create_info.image                           = vk_obj->image;
	create_info.viewType                        = VK_IMAGE_VIEW_TYPE_2D;
	create_info.format                          = vk_obj->format;
	create_info.components.r                    = VK_COMPONENT_SWIZZLE_IDENTITY;
	create_info.components.g                    = VK_COMPONENT_SWIZZLE_IDENTITY;
	create_info.components.b                    = VK_COMPONENT_SWIZZLE_IDENTITY;
	create_info.components.a                    = VK_COMPONENT_SWIZZLE_IDENTITY;
	create_info.subresourceRange.aspectMask     = VK_IMAGE_ASPECT_COLOR_BIT;
	create_info.subresourceRange.baseArrayLayer = 0;
	create_info.subresourceRange.baseMipLevel   = 0;
	create_info.subresourceRange.layerCount     = 1;
	create_info.subresourceRange.levelCount     = 1;

	vkCreateImageView(ctx->device, &create_info, nullptr, &vk_obj->image_view);

	EXIT_NOT_IMPLEMENTED(vk_obj->image_view == nullptr);

	return vk_obj;
}

static bool buffer_is_tiled(uint64_t vaddr, uint64_t size)
{
	if ((size & 0x7u) == 0)
	{
		const auto* ptr     = reinterpret_cast<const uint64_t*>(vaddr);
		const auto* ptr_end = reinterpret_cast<const uint64_t*>(vaddr + size / 8);
		for (uint64_t element = *ptr; ptr < ptr_end; ptr++)
		{
			if (element != *ptr)
			{
				return true;
			}
		}
		return false;
	}
	return true;
}

static void update_func(GraphicContext* ctx, const uint64_t* params, void* obj, const uint64_t* vaddr, const uint64_t* size, int vaddr_num)
{
	KYTY_PROFILER_BLOCK("VideoOutBufferObject::update_func");

	EXIT_IF(obj == nullptr);
	EXIT_IF(ctx == nullptr);
	EXIT_IF(params == nullptr);
	EXIT_IF(vaddr == nullptr || size == nullptr || vaddr_num != 1);

	auto* vk_obj = static_cast<VideoOutVulkanImage*>(obj);

	bool tiled = (params[VideoOutBufferObject::PARAM_TILED] != 0);
	bool neo   = (params[VideoOutBufferObject::PARAM_NEO] != 0);

	if (tiled && buffer_is_tiled(*vaddr, *size))
	{
		auto* temp_buf = new uint8_t[*size];
		TileConvertTiledToLinear(temp_buf, reinterpret_cast<void*>(*vaddr), TileMode::VideoOutTiled,
		                         params[VideoOutBufferObject::PARAM_WIDTH], params[VideoOutBufferObject::PARAM_HEIGHT], neo);
		UtilFillImage(ctx, vk_obj, temp_buf, *size);
		delete[] temp_buf;
	} else
	{
		UtilFillImage(ctx, vk_obj, reinterpret_cast<void*>(*vaddr), *size);
	}
}

bool VideoOutBufferObject::Equal(const uint64_t* other) const
{
	return (params[PARAM_FORMAT] == other[PARAM_FORMAT] && params[PARAM_WIDTH] == other[PARAM_WIDTH] &&
	        params[PARAM_HEIGHT] == other[PARAM_HEIGHT] && params[PARAM_TILED] == other[PARAM_TILED]);
}

static void delete_func(GraphicContext* ctx, void* obj, VulkanMemory* mem)
{
	KYTY_PROFILER_BLOCK("VideoOutBufferObject::delete_func");

	auto* vk_obj = reinterpret_cast<VideoOutVulkanImage*>(obj);

	EXIT_IF(vk_obj == nullptr);
	EXIT_IF(ctx == nullptr);

	// if (vk_obj->framebuffer != nullptr)
	{
		DeleteFramebuffer(vk_obj);
	}

	vkDestroyImageView(ctx->device, vk_obj->image_view, nullptr);

	vkDestroyImage(ctx->device, vk_obj->image, nullptr);

	VulkanFree(ctx, mem);

	delete vk_obj;
}

GpuObject::delete_func_t VideoOutBufferObject::GetDeleteFunc() const
{
	return delete_func;
}

GpuObject::update_func_t VideoOutBufferObject::GetUpdateFunc() const
{
	return update_func;
}

} // namespace Kyty::Libs::Graphics

#endif // KYTY_EMU_ENABLED
