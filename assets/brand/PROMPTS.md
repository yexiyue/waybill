# waybill 图像生成记录

生成方式：内置 image_gen。日期：2026-10-01。

吉祥物沿用仓库既有 Bill / 雁哥设定；头像、头图和封面使用吉祥物作身份参考。
PNG 为生成原图，未用脚本修改画面。

## mascot

新图，无输入参考；透明背景。

```text
Use case: logo-brand
Asset type: primary transparent mascot illustration for the open-source Rust library waybill.
Primary request: Create Bill, a courier goose, the established waybill mascot. A memorable friendly but competent migratory goose, full body standing in a slightly forward-moving three-quarter pose. Slim curved neck, light warm grey and cream feathers, charcoal wing tips and dark expressive eyes, muted orange beak and feet. A small cylindrical waybill/document tube securely strapped to one leg is the signature identifier; a tiny simple courier shoulder strap may hold the document tube but do not add extra bags or props. Let the leg-mounted tube be clearly visible. Clean clever silhouette with generous space around the bird.
Style/medium: polished flat editorial illustration, crisp confident contours, very restrained soft shading, subtle paper-print character inside shapes. Contemporary independent open-source identity; charming without baby-like proportions, plush 3D, anime or generic tech robot styling.
Scene/backdrop: genuinely transparent background. One isolated mascot; no ground plane, no background pattern, no checkerboard painted into the image.
Palette: natural goose colours from the established courier-goose concept: cream and warm grey feathers, charcoal ink details, subdued orange beak and feet; small deep teal courier tube.
Composition: square canvas, full body and all extremities inside frame, mascot occupies about 75 percent of height. Readable at small sizes.
Text: none.
Constraints: exactly one goose, anatomically coherent single head, two wings and two legs. No lettering, watermarks, logos, arrows, cloud or disk icons, badges, sticker border or surrounding scene. Preserve real alpha transparency.
```

## banner

参考：bill-mascot-v1.png。

```text
Use case: logo-brand
Asset type: wide README hero/banner for the Rust library waybill, approximately 3:1 aspect ratio.
Input image 1: reference image for Bill the courier-goose mascot; use his same visual identity, natural cream and grey feathers, long neck, charcoal wing tips, orange bill/feet and deep teal tube strapped to his leg. It is a mascot identity reference, not a request to make a collage.
Primary request: Design a polished open-source library masthead with one large, impeccably typeset lowercase wordmark "waybill" and Bill standing confidently. Elegant practical dispatch-station / printed-waybill visual language, restrained editorial layout on a warm off-white background, charcoal ink lettering, deep teal accents matching the tube. A subtle thin dashed route can traverse the background with two understated checkpoints, connecting the delivery theme without becoming a diagram.
Composition: wide landscape, bold wordmark grouped on the left and one full-body goose toward the right, ample calm negative space and generous padding. This supports README masthead reading. Lettering and bird must feel balanced, no cramped details. Optional subtle ruled-paper lines near the edges only.
Text (verbatim): "waybill". Spell w-a-y-b-i-l-l in lowercase. No tagline or additional text; no status claims.
Style: match reference bird's fine editorial illustration and paper texture, use crisp clean graphic typography with warm rounded grotesk letterforms. Mature independent developer project; avoid generic neon tech aesthetics.
Constraints: keep complete mascot and its leg-mounted document tube visible. No additional birds, logo marks of cloud providers, gradients, fake UI, screenshots, promotional badges, watermark, decorative tiles or multiple panels. Opaque warm cream backdrop.
```

## avatar

参考：bill-mascot-v1.png。

```text
Use case: logo-brand
Asset type: square avatar / simplified brand mark for waybill, transparent outside the mark.
Input image 1: mascot identity reference. Preserve Bill's natural warm-grey crown, cream face, long goose neck, dark expressive eye and muted orange bill.
Primary request: Translate Bill the courier goose into a confident minimal head-and-curved-neck portrait emblem suitable for a GitHub avatar and small documentation icon. Bold simplified flat shapes, a dark charcoal contour, subtle natural feather colour regions only, friendly attentive expression. Use a deep teal circular backdrop matching the document tube in the reference. Keep the emblem silhouette strong at 32px; no fine feather texture. A graceful long neck must make it clearly a goose.
Composition: one single centered circular emblem on square canvas, all outer space genuinely transparent. Goose face in three-quarter view, generously sized, clean breathing room within circle.
Text: none.
Constraints: one goose portrait, no extra mascot, no letters, no document tube floating around the face, no sticker border, no scenery, no mockup, no shadow beyond circle. Actual alpha transparency outside circle; preserve the mascot's identity while simplifying the illustration.
```

## social

参考：bill-mascot-v1.png。

```text
Use case: ads-marketing
Asset type: landscape social / repository cover card for waybill, approximately 1.9:1 aspect ratio.
Input image 1: visual identity reference for Bill, the courier goose. Use the same goose shape, long neck, cream-grey feathers, orange bill and feet, charcoal wing tips, teal cylindrical waybill tube strapped to one leg.
Primary request: A refined graphic social card for an independent Rust open-source project. Deep teal field, cream ink type, natural orange accents from the bird. Bill stands on the right with a small restrained dashed route behind him; clear editorial typography on left. Confident, warm and useful; print-inspired minimalism with a little paper grain. One composed scene, not a branding board.
Text (verbatim): large lowercase "waybill"; smaller two-line subtitle "Resumable delivery." and "Both ways." Exact spelling and punctuation only. No other text.
Composition: wide landscape, generous outer margin, large legible wordmark and compact subtitle, full mascot visible with tube attached to leg. Adapt bird illustration for good contrast while retaining reference colours.
Constraints: no feature claims implying released implementation, cloud-provider logos, extra characters, badges, invented UI, QR code, watermark or multiple panels. Use an opaque deep teal background, consistent with mascot tube. Crisp readable lettering.
```

## 社交封面字形统一

输入 1：社交封面生成草稿（未选用，保留在生成工具默认目录）。输入 2：readme-banner-v1.png 字形参考。最终选用此修订的输出。

```text
Use case: precise-object-edit
Asset type: final waybill social cover, approximately 1.9:1 landscape.
Input image 1: EDIT TARGET, the teal social cover with Bill the goose and cream serif text.
Input image 2: TYPE STYLE REFERENCE, the cream README banner with a bold rounded sans-serif "waybill" wordmark.
Change only the typography of image 1: replace its serif wordmark with the same heavy rounded sans-serif lowercase wordmark style as image 2; replace the two subtitle lines with a clean matching sans-serif. Exact text must stay "waybill", "Resumable delivery.", "Both ways." Maintain the same hierarchy and approximate text placement.
Keep the mascot exactly as in image 1, the tube strapped to his leg, the route, orange location pin, deep teal background, cream lettering colour, all margins, scale, composition and aspect ratio. No additional text or elements. Preserve the illustration and background, change only the type so this is one coherent brand identity with the README banner. Output opaque PNG.
```
