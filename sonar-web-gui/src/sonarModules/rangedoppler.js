// / src/sonarModules/rangedoppler.js
export class RangeDopplerDisplay {
  constructor(canvas) {
    this.canvas = canvas;
    this.ctx = canvas ? canvas.getContext("2d") : null;
  }

  updateDimensions(width, height) {
    if (!this.canvas) return;
    
    this.width = width;
    this.height = height;
    this.canvas.width = width;
    this.canvas.height = height;
    this.imagedata = new ImageData(width, height);
    this.imgbuffer = this.imagedata.data;

    for (let x = 0; x < this.width; x++) {
      for (let y = 0; y < this.height; y++) {
        this.imgbuffer[x * this.height * 4 + y * 4 + 3] = 256;
      }
    }
  }

  draw(array) {
    if (!this.canvas || !this.ctx || !this.imgbuffer) {
      console.error("Canvas not initialized");
      return;
    }
    
    if (array.length !== this.width * this.height) {
      console.error(
        `Array length (${array.length}) doesn't match dimensions ${this.width}x${this.height}`,
      );
      return;
    }

    const max = Math.max(...array);

    for (let x = 0; x < this.width; x++) {
      for (let y = 0; y < this.height; y++) {
        const val = array[x * this.height + y] / max;
        this.imgbuffer[x * this.height * 4 + y * 4 + 0] = Math.round(val * 256);
      }
    }
    this.ctx.putImageData(this.imagedata, 0, 0);
    return max;
  }
}